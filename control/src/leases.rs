//! The control plane's state, in Postgres (`db/postgres`): the sessions, and who may use each
//! (`session_access`, which chlive grants from): its owner, its viewers, and the one player
//! holding its lease; and what users keep with control, sealed (`seal`): their ChatGPT links,
//! API keys and secrets.
//! A lease row's `expires_at` is null while its player is connected; once it isn't, the player
//! may come back for it until then, and past it the row is deleted ([`Store::sweep`]). A session
//! with a lease is active; one without, idle. Control starts a player by reserving the lease for
//! it ([`Store::reserve`]), which the player then takes as its own.

use std::time::Duration;

use anyhow::Context;
use dirigent_protocol::control::{Holder, Resources, Role, SessionInfo, SessionSpec};
use tokio::sync::Mutex;
use tokio_postgres::{Client, NoTls, Row};
use tracing::error;
use uuid::Uuid;

pub struct Store {
    /// One connection, in turns: transactions need it to themselves.
    db: Mutex<Client>,
    /// How long a dropped player keeps its lease, in seconds.
    grace: f64,
}

/// What putting a session did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Put {
    Made,
    /// It was there, with the same spec.
    Exists,
    /// It was there, with another spec: nothing changed.
    Conflict,
    /// It was there, another's: nothing changed.
    NotOwner,
}

/// What a player asking for a session gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquire {
    /// It holds it, as the `epoch`th player, and runs the spec, for its owner.
    Acquired(i64, SessionSpec, String),
    /// Another player does.
    Held,
    /// There's no such session.
    Unknown,
    /// The lease it resumes is gone.
    Lost,
    /// The session is archived.
    Archived,
}

/// What reserving a session's lease for a player control starts did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reserve {
    /// For the `epoch`th player.
    Reserved(i64),
    /// A player holds it already.
    Held,
    Unknown,
    Archived,
}

/// What deleting a session did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delete {
    /// It's marked: control removes what it left.
    Marked,
    /// It's archived first.
    NotArchived,
    Unknown,
}

/// What archiving a session (or unarchiving it) did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Archive {
    Done,
    /// A player holds it, which must stop first.
    Active,
    Unknown,
}

impl Store {
    /// Connected to `url`; the process exits if the connection ends.
    pub async fn connect(url: &str, grace: Duration) -> anyhow::Result<Self> {
        let (db, conn) = tokio_postgres::connect(url, NoTls)
            .await
            .context("connecting to Postgres")?;
        tokio::spawn(async move {
            if let Err(e) = conn.await {
                error!("the Postgres connection failed: {e}");
            }
            std::process::exit(1);
        });
        Ok(Store {
            db: Mutex::new(db),
            grace: grace.as_secs_f64(),
        })
    }

    /// The session `id`, made with `spec` and `owner` (a subject) if there's none, with
    /// `viewers` besides those it has. One that's there keeps its spec and owner: given another,
    /// nothing changes.
    pub async fn put_session(
        &self,
        id: Uuid,
        spec: &SessionSpec,
        owner: &str,
        viewers: &[String],
    ) -> anyhow::Result<Put> {
        let json = serde_json::to_value(spec)?;
        let mut db = self.db.lock().await;
        let tx = db.transaction().await?;
        let made = tx
            .execute(
                "INSERT INTO dirigent.sessions (id, spec) VALUES ($1, $2) ON CONFLICT DO NOTHING",
                &[&id, &json],
            )
            .await?;
        let put = if made == 1 {
            tx.execute(
                "INSERT INTO dirigent.session_access (session_id, subject, role) \
                 VALUES ($1, $2, 'owner')",
                &[&id, &owner],
            )
            .await?;
            Put::Made
        } else {
            let row = tx
                .query_one(
                    "SELECT spec, (SELECT subject FROM dirigent.session_access \
                       WHERE session_id = $1 AND role = 'owner') \
                     FROM dirigent.sessions WHERE id = $1 FOR UPDATE",
                    &[&id],
                )
                .await?;
            if row.get::<_, Option<&str>>(1) != Some(owner) {
                return Ok(Put::NotOwner);
            }
            let had: SessionSpec = serde_json::from_value(row.get(0))?;
            if had != *spec {
                return Ok(Put::Conflict);
            }
            Put::Exists
        };
        for viewer in viewers.iter().filter(|v| *v != owner) {
            tx.execute(
                "INSERT INTO dirigent.session_access (session_id, subject, role) \
                 VALUES ($1, $2, 'viewer') ON CONFLICT DO NOTHING",
                &[&id, viewer],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(put)
    }

    /// The session `id`, if there's one `caller` (a subject) owns or views.
    pub async fn session(&self, id: Uuid, caller: &str) -> anyhow::Result<Option<SessionInfo>> {
        let rows = self
            .db
            .lock()
            .await
            .query(&format!("{SESSIONS} WHERE s.id = $2"), &[&caller, &id])
            .await?;
        rows.iter().map(session_info).next().transpose()
    }

    /// The sessions `caller` owns or views.
    pub async fn sessions(&self, caller: &str) -> anyhow::Result<Vec<SessionInfo>> {
        let rows = self
            .db
            .lock()
            .await
            .query(&format!("{SESSIONS} ORDER BY s.id"), &[&caller])
            .await?;
        rows.iter().map(session_info).collect()
    }

    /// `caller`'s role in the session `id`, if it has one (or there's no such session).
    pub async fn role(&self, id: Uuid, caller: &str) -> anyhow::Result<Option<Role>> {
        let row = self
            .db
            .lock()
            .await
            .query_opt(
                "SELECT role FROM dirigent.session_access \
                 WHERE session_id = $1 AND subject = $2 AND role IN ('owner', 'viewer')",
                &[&id, &caller],
            )
            .await?;
        Ok(row.map(|r| role(r.get(0))))
    }

    /// Let `viewer` read the session `id` (unless it has a role there already).
    pub async fn add_viewer(&self, id: Uuid, viewer: &str) -> anyhow::Result<()> {
        self.db
            .lock()
            .await
            .execute(
                "INSERT INTO dirigent.session_access (session_id, subject, role) \
                 VALUES ($1, $2, 'viewer') ON CONFLICT DO NOTHING",
                &[&id, &viewer],
            )
            .await?;
        Ok(())
    }

    /// `viewer` reads the session `id` no more.
    pub async fn remove_viewer(&self, id: Uuid, viewer: &str) -> anyhow::Result<()> {
        self.db
            .lock()
            .await
            .execute(
                "DELETE FROM dirigent.session_access \
                 WHERE session_id = $1 AND subject = $2 AND role = 'viewer'",
                &[&id, &viewer],
            )
            .await?;
        Ok(())
    }

    /// `subject` (a player) asks to hold `session`, or to resume the lease it held, `resume`.
    /// Its own lease, connected or not, is its own again; another's that hasn't expired keeps
    /// the session; otherwise it's taken, with the session's next epoch.
    pub async fn acquire(
        &self,
        session: Uuid,
        subject: &str,
        resume: Option<i64>,
    ) -> anyhow::Result<Acquire> {
        let mut db = self.db.lock().await;
        let tx = db.transaction().await?;
        let Some(row) = tx
            .query_opt(
                "SELECT epoch, spec, archived IS NOT NULL FROM dirigent.sessions \
                 WHERE id = $1 FOR UPDATE",
                &[&session],
            )
            .await?
        else {
            return Ok(match resume {
                Some(_) => Acquire::Lost,
                None => Acquire::Unknown,
            });
        };
        // an archived session has no lease to resume
        if row.get::<_, bool>(2) {
            return Ok(Acquire::Archived);
        }
        let epoch: i64 = row.get(0);
        let spec: SessionSpec = serde_json::from_value(row.get(1))?;
        let owner: String = tx
            .query_opt(
                "SELECT subject FROM dirigent.session_access \
                 WHERE session_id = $1 AND role = 'owner'",
                &[&session],
            )
            .await?
            .map(|r| r.get(0))
            .unwrap_or_default();
        let holder = tx
            .query_opt(
                "SELECT subject, expires_at IS NULL OR expires_at > now() \
                 FROM dirigent.session_access WHERE session_id = $1 AND role = 'player'",
                &[&session],
            )
            .await?;
        let got = match holder {
            Some(h) if h.get::<_, &str>(0) == subject => {
                tx.execute(
                    "UPDATE dirigent.session_access SET expires_at = NULL \
                     WHERE session_id = $1 AND role = 'player'",
                    &[&session],
                )
                .await?;
                Acquire::Acquired(epoch, spec, owner)
            }
            _ if resume.is_some() => Acquire::Lost,
            Some(h) if h.get::<_, bool>(1) => Acquire::Held,
            _ => {
                tx.execute(
                    "DELETE FROM dirigent.session_access WHERE session_id = $1 AND role = 'player'",
                    &[&session],
                )
                .await?;
                let epoch: i64 = tx
                    .query_one(
                        "UPDATE dirigent.sessions SET epoch = epoch + 1 WHERE id = $1 \
                         RETURNING epoch",
                        &[&session],
                    )
                    .await?
                    .get(0);
                tx.execute(
                    "INSERT INTO dirigent.session_access (session_id, subject, role) \
                     VALUES ($1, $2, 'player')",
                    &[&session, &subject],
                )
                .await?;
                Acquire::Acquired(epoch, spec, owner)
            }
        };
        tx.commit().await?;
        Ok(got)
    }

    /// `subject` gives its lease on `session` back.
    pub async fn release(&self, session: Uuid, subject: &str) -> anyhow::Result<()> {
        self.db
            .lock()
            .await
            .execute(
                "DELETE FROM dirigent.session_access \
                 WHERE session_id = $1 AND subject = $2 AND role = 'player'",
                &[&session, &subject],
            )
            .await?;
        Ok(())
    }

    /// `subject` dropped: its lease on `session` expires after the grace period.
    pub async fn detach(&self, session: Uuid, subject: &str) -> anyhow::Result<()> {
        self.db
            .lock()
            .await
            .execute(
                "UPDATE dirigent.session_access SET expires_at = now() + make_interval(secs => $3) \
                 WHERE session_id = $1 AND subject = $2 AND role = 'player' AND expires_at IS NULL",
                &[&session, &subject, &self.grace],
            )
            .await?;
        Ok(())
    }

    /// At start: the players connected to the previous run get the grace period to reconnect;
    /// how many there were.
    pub async fn detach_all(&self) -> anyhow::Result<u64> {
        Ok(self
            .db
            .lock()
            .await
            .execute(
                "UPDATE dirigent.session_access SET expires_at = now() + make_interval(secs => $1) \
                 WHERE role = 'player' AND expires_at IS NULL",
                &[&self.grace],
            )
            .await?)
    }

    /// Delete the leases that expired (chlive's grants go with them), of players started by
    /// `runner` or not by control at all: each one's session, and whether `runner` had started
    /// its player. (Those of other runners are their controls' to sweep.)
    pub async fn sweep(&self, runner: &str) -> anyhow::Result<Vec<(Uuid, bool)>> {
        let rows = self
            .db
            .lock()
            .await
            .query(
                "DELETE FROM dirigent.session_access WHERE role = 'player' AND expires_at < now() \
                 AND (spawned_by IS NULL OR spawned_by = $1) \
                 RETURNING session_id, spawned_by IS NOT NULL",
                &[&runner],
            )
            .await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    /// The lease on `session` for `subject`, a player control is about to start with `runner`,
    /// unless one holds it (an expired lease doesn't): the player has `startup` to take it.
    pub async fn reserve(
        &self,
        session: Uuid,
        subject: &str,
        runner: &str,
        startup: Duration,
    ) -> anyhow::Result<Reserve> {
        let mut db = self.db.lock().await;
        let tx = db.transaction().await?;
        let found = tx
            .query_opt(
                "SELECT archived IS NOT NULL FROM dirigent.sessions WHERE id = $1 FOR UPDATE",
                &[&session],
            )
            .await?;
        match found.map(|r| r.get::<_, bool>(0)) {
            None => return Ok(Reserve::Unknown),
            Some(true) => return Ok(Reserve::Archived),
            Some(false) => {}
        }
        let held = tx
            .query_opt(
                "SELECT expires_at IS NULL OR expires_at > now() \
                 FROM dirigent.session_access WHERE session_id = $1 AND role = 'player'",
                &[&session],
            )
            .await?;
        match held.map(|r| r.get::<_, bool>(0)) {
            Some(true) => return Ok(Reserve::Held),
            Some(false) => {
                tx.execute(
                    "DELETE FROM dirigent.session_access WHERE session_id = $1 AND role = 'player'",
                    &[&session],
                )
                .await?;
            }
            None => {}
        }
        let epoch: i64 = tx
            .query_one(
                "UPDATE dirigent.sessions SET epoch = epoch + 1 WHERE id = $1 RETURNING epoch",
                &[&session],
            )
            .await?
            .get(0);
        tx.execute(
            "INSERT INTO dirigent.session_access \
             (session_id, subject, role, spawned_by, expires_at) \
             VALUES ($1, $2, 'player', $3, now() + make_interval(secs => $4))",
            &[&session, &subject, &runner, &startup.as_secs_f64()],
        )
        .await?;
        tx.commit().await?;
        Ok(Reserve::Reserved(epoch))
    }

    /// The machine `session`'s spec asks for, if it asks for one (and the session exists).
    pub async fn machine(&self, session: Uuid) -> anyhow::Result<Option<Resources>> {
        let row = self
            .db
            .lock()
            .await
            .query_opt(
                "SELECT spec -> 'resources' FROM dirigent.sessions WHERE id = $1",
                &[&session],
            )
            .await?;
        match row.and_then(|r| r.get::<_, Option<serde_json::Value>>(0)) {
            Some(v) => Ok(Some(serde_json::from_value(v)?)),
            None => Ok(None),
        }
    }

    /// The settings (as kept, if any) of each user who made a session within `within`.
    pub async fn active_owners(
        &self,
        within: Duration,
    ) -> anyhow::Result<Vec<Option<serde_json::Value>>> {
        let rows = self
            .db
            .lock()
            .await
            .query(
                "SELECT DISTINCT a.subject, u.settings FROM dirigent.session_access a \
                 JOIN dirigent.sessions s ON s.id = a.session_id \
                 LEFT JOIN dirigent.user_settings u ON u.subject = a.subject \
                 WHERE a.role = 'owner' AND s.created > now() - make_interval(secs => $1)",
                &[&within.as_secs_f64()],
            )
            .await?;
        Ok(rows.iter().map(|r| r.get(1)).collect())
    }

    /// The players holding leases (or away, within the grace period) that `runner` started.
    pub async fn players(&self, runner: &str) -> anyhow::Result<Vec<String>> {
        let rows = self
            .db
            .lock()
            .await
            .query(
                "SELECT subject FROM dirigent.session_access \
                 WHERE role = 'player' AND spawned_by = $1",
                &[&runner],
            )
            .await?;
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    /// Delete the lease on `session`, whoever holds it; whether there was one.
    pub async fn drop_lease(&self, session: Uuid) -> anyhow::Result<bool> {
        let n = self
            .db
            .lock()
            .await
            .execute(
                "DELETE FROM dirigent.session_access WHERE session_id = $1 AND role = 'player'",
                &[&session],
            )
            .await?;
        Ok(n == 1)
    }

    /// Archive the session `id` (`on`), unless a player holds it, or unarchive it. An archived
    /// one keeps when it was archived, and has no lease: an expired one goes with archiving.
    pub async fn archive(&self, id: Uuid, on: bool) -> anyhow::Result<Archive> {
        let mut db = self.db.lock().await;
        let tx = db.transaction().await?;
        let found = tx
            .query_opt(
                "SELECT 1 FROM dirigent.sessions WHERE id = $1 FOR UPDATE",
                &[&id],
            )
            .await?;
        if found.is_none() {
            return Ok(Archive::Unknown);
        }
        if on {
            let held = tx
                .query_opt(
                    "SELECT expires_at IS NULL OR expires_at > now() \
                     FROM dirigent.session_access WHERE session_id = $1 AND role = 'player'",
                    &[&id],
                )
                .await?;
            match held.map(|r| r.get::<_, bool>(0)) {
                Some(true) => return Ok(Archive::Active),
                Some(false) => {
                    tx.execute(
                        "DELETE FROM dirigent.session_access \
                         WHERE session_id = $1 AND role = 'player'",
                        &[&id],
                    )
                    .await?;
                }
                None => {}
            }
        }
        tx.execute(
            "UPDATE dirigent.sessions \
             SET archived = CASE WHEN $2 THEN coalesce(archived, now()) END WHERE id = $1",
            &[&id, &on],
        )
        .await?;
        tx.commit().await?;
        Ok(Archive::Done)
    }

    /// Delete the session `id`, archived: it's marked, and no one has access any more (chlive's
    /// grants go with the rows); [`Store::deleting`] lists it until [`Store::deleted`].
    pub async fn delete(&self, id: Uuid) -> anyhow::Result<Delete> {
        let mut db = self.db.lock().await;
        let tx = db.transaction().await?;
        let found = tx
            .query_opt(
                "SELECT archived IS NOT NULL FROM dirigent.sessions WHERE id = $1 FOR UPDATE",
                &[&id],
            )
            .await?;
        match found.map(|r| r.get::<_, bool>(0)) {
            None => return Ok(Delete::Unknown),
            Some(false) => return Ok(Delete::NotArchived),
            Some(true) => {}
        }
        tx.execute(
            "UPDATE dirigent.sessions SET deleting = coalesce(deleting, now()) WHERE id = $1",
            &[&id],
        )
        .await?;
        tx.execute(
            "DELETE FROM dirigent.session_access WHERE session_id = $1",
            &[&id],
        )
        .await?;
        tx.commit().await?;
        Ok(Delete::Marked)
    }

    /// The sessions being deleted, the oldest first: each one's id, and whether it was marked
    /// more than `settled` ago.
    pub async fn deleting(&self, settled: Duration) -> anyhow::Result<Vec<(Uuid, bool)>> {
        let rows = self
            .db
            .lock()
            .await
            .query(
                "SELECT id, deleting < now() - make_interval(secs => $1) FROM dirigent.sessions \
                 WHERE deleting IS NOT NULL ORDER BY deleting",
                &[&settled.as_secs_f64()],
            )
            .await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    /// The session `id`, being deleted, is gone: its row goes last (its secrets with it).
    pub async fn deleted(&self, id: Uuid) -> anyhow::Result<()> {
        self.db
            .lock()
            .await
            .execute(
                "DELETE FROM dirigent.sessions WHERE id = $1 AND deleting IS NOT NULL",
                &[&id],
            )
            .await?;
        Ok(())
    }

    /// A web sign-in for `subject`, until `ttl` from now (and as long again from each use): its
    /// id, with `secret`'s SHA-256 kept for it. Their sign-ins that expired go.
    pub async fn open_web_session(
        &self,
        subject: &str,
        secret: &[u8],
        ttl: Duration,
    ) -> anyhow::Result<Uuid> {
        let id = Uuid::new_v4();
        let db = self.db.lock().await;
        db.execute(
            "DELETE FROM dirigent.web_sessions WHERE subject = $1 AND expires < now()",
            &[&subject],
        )
        .await?;
        db.execute(
            "INSERT INTO dirigent.web_sessions (id, subject, secret, expires) \
             VALUES ($1, $2, $3, now() + make_interval(secs => $4))",
            &[&id, &subject, &sha256(secret), &ttl.as_secs_f64()],
        )
        .await?;
        Ok(id)
    }

    /// Who the web sign-in `id` is, if `secret` is its own and it hasn't expired: it's good for
    /// `ttl` from now on.
    pub async fn web_session(
        &self,
        id: Uuid,
        secret: &[u8],
        ttl: Duration,
    ) -> anyhow::Result<Option<String>> {
        let row = self
            .db
            .lock()
            .await
            .query_opt(
                "UPDATE dirigent.web_sessions SET expires = now() + make_interval(secs => $3) \
                 WHERE id = $1 AND secret = $2 AND expires > now() RETURNING subject",
                &[&id, &sha256(secret), &ttl.as_secs_f64()],
            )
            .await?;
        Ok(row.map(|r| r.get(0)))
    }

    /// The web sign-in `id` is over.
    pub async fn close_web_session(&self, id: Uuid) -> anyhow::Result<()> {
        self.db
            .lock()
            .await
            .execute("DELETE FROM dirigent.web_sessions WHERE id = $1", &[&id])
            .await?;
        Ok(())
    }

    /// `subject` may use these GitHub App installations (id, account), besides those they could
    /// already.
    pub async fn link_github(
        &self,
        subject: &str,
        installations: &[(i64, String)],
    ) -> anyhow::Result<()> {
        let db = self.db.lock().await;
        for (id, account) in installations {
            db.execute(
                "INSERT INTO dirigent.github_installations (subject, installation_id, account) \
                 VALUES ($1, $2, $3) ON CONFLICT (subject, installation_id) \
                 DO UPDATE SET account = excluded.account, linked = now()",
                &[&subject, id, account],
            )
            .await?;
        }
        Ok(())
    }

    /// The GitHub App installations `subject` may use: (id, account), by account.
    pub async fn github_installations(&self, subject: &str) -> anyhow::Result<Vec<(i64, String)>> {
        let rows = self
            .db
            .lock()
            .await
            .query(
                "SELECT installation_id, account FROM dirigent.github_installations \
                 WHERE subject = $1 ORDER BY account",
                &[&subject],
            )
            .await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    /// `subject` uses no GitHub App installations any more; whether they did.
    pub async fn unlink_github(&self, subject: &str) -> anyhow::Result<bool> {
        let n = self
            .db
            .lock()
            .await
            .execute(
                "DELETE FROM dirigent.github_installations WHERE subject = $1",
                &[&subject],
            )
            .await?;
        Ok(n > 0)
    }

    /// `subject`'s secret `name` for all their sessions, sealed, if they gave one.
    pub async fn user_secret(&self, subject: &str, name: &str) -> anyhow::Result<Option<Vec<u8>>> {
        let row = self
            .db
            .lock()
            .await
            .query_opt(
                "SELECT value FROM dirigent.user_secrets WHERE subject = $1 AND name = $2",
                &[&subject, &name],
            )
            .await?;
        Ok(row.map(|r| r.get(0)))
    }

    /// `subject`'s settings as kept, if they saved any.
    pub async fn settings(&self, subject: &str) -> anyhow::Result<Option<serde_json::Value>> {
        let row = self
            .db
            .lock()
            .await
            .query_opt(
                "SELECT settings FROM dirigent.user_settings WHERE subject = $1",
                &[&subject],
            )
            .await?;
        Ok(row.map(|r| r.get(0)))
    }

    /// `subject`'s settings are these now.
    pub async fn put_settings(
        &self,
        subject: &str,
        settings: &serde_json::Value,
    ) -> anyhow::Result<()> {
        self.db
            .lock()
            .await
            .execute(
                "INSERT INTO dirigent.user_settings (subject, settings) VALUES ($1, $2) \
                 ON CONFLICT (subject) DO UPDATE SET settings = excluded.settings, updated = now()",
                &[&subject, settings],
            )
            .await?;
        Ok(())
    }

    /// The players' system built (or being built) for `hash`, if one was.
    pub async fn player_system(&self, hash: &str) -> anyhow::Result<Option<PlayerSystem>> {
        let row = self
            .db
            .lock()
            .await
            .query_opt(
                "SELECT state, build_id, store_path, error FROM dirigent.player_systems \
                 WHERE hash = $1",
                &[&hash],
            )
            .await?;
        Ok(row.map(|r| PlayerSystem {
            state: r.get(0),
            build: r.get(1),
            store_path: r.get(2),
            error: r.get(3),
        }))
    }

    /// `hash`'s system is being built, as `build`, which reports with the token whose sha256 is
    /// `token` (what was there for it before is replaced).
    pub async fn build_system(&self, hash: &str, build: Uuid, token: &str) -> anyhow::Result<()> {
        self.db
            .lock()
            .await
            .execute(
                "INSERT INTO dirigent.player_systems (hash, state, build_id, token) \
                 VALUES ($1, 'building', $2, $3) ON CONFLICT (hash) DO UPDATE SET \
                 state = 'building', build_id = $2, token = $3, store_path = NULL, error = NULL, \
                 started = now(), updated = now()",
                &[&hash, &build, &token],
            )
            .await?;
        Ok(())
    }

    /// The build `build` is done, as told with the token whose sha256 is `token`: its system's
    /// store path, or why it failed. Whether it was a build under way, with that token (each is
    /// done once).
    pub async fn system_built(
        &self,
        build: Uuid,
        token: &str,
        result: Result<&str, &str>,
    ) -> anyhow::Result<bool> {
        let (state, path, error) = match result {
            Ok(path) => ("ready", Some(path), None),
            Err(error) => ("failed", None, Some(error)),
        };
        let n = self
            .db
            .lock()
            .await
            .execute(
                "UPDATE dirigent.player_systems SET state = $3, store_path = $4, error = $5, \
                 updated = now() WHERE build_id = $1 AND token = $2 AND state = 'building'",
                &[&build, &token, &state, &path, &error],
            )
            .await?;
        Ok(n == 1)
    }

    /// The build `build` failed without telling (it timed out, or it couldn't be started):
    /// unless it's done already. Whether it was under way.
    pub async fn system_failed(&self, build: Uuid, why: &str) -> anyhow::Result<bool> {
        let n = self
            .db
            .lock()
            .await
            .execute(
                "UPDATE dirigent.player_systems SET state = 'failed', error = $2, \
                 updated = now() WHERE build_id = $1 AND state = 'building'",
                &[&build, &why],
            )
            .await?;
        Ok(n == 1)
    }

    /// `hash`'s system is broken: a session's VM didn't come up with it.
    pub async fn system_broken(&self, hash: &str, why: &str) -> anyhow::Result<()> {
        self.db
            .lock()
            .await
            .execute(
                "UPDATE dirigent.player_systems SET state = 'broken', error = $2, \
                 updated = now() WHERE hash = $1 AND state = 'ready'",
                &[&hash, &why],
            )
            .await?;
        Ok(())
    }

    /// Store paths players' VMs added to the shared Nix store, and whether upstream signed each.
    pub async fn nix_added(&self, paths: &[(String, bool)]) -> anyhow::Result<()> {
        let (names, signed): (Vec<&str>, Vec<bool>) =
            paths.iter().map(|(p, s)| (p.as_str(), *s)).unzip();
        self.db
            .lock()
            .await
            .execute(
                "INSERT INTO dirigent.nix_paths (path, signed) \
                 SELECT * FROM unnest($1::text[], $2::bool[]) ON CONFLICT (path) DO UPDATE \
                 SET uses = nix_paths.uses + 1, signed = nix_paths.signed OR excluded.signed",
                &[&names, &signed],
            )
            .await?;
        Ok(())
    }

    /// The paths no generation has yet: those upstream signed, or (`all`) every one; the most
    /// used first, 20000 at most.
    pub async fn unpromoted(&self, all: bool) -> anyhow::Result<Vec<(String, bool)>> {
        let rows = self
            .db
            .lock()
            .await
            .query(
                "SELECT path, signed FROM dirigent.nix_paths \
                 WHERE promoted_in IS NULL AND (signed OR $1) ORDER BY uses DESC LIMIT 20000",
                &[&all],
            )
            .await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    /// `paths` are in the shared store's generation `generation`.
    pub async fn promoted(&self, paths: &[String], generation: i64) -> anyhow::Result<()> {
        self.db
            .lock()
            .await
            .execute(
                "UPDATE dirigent.nix_paths SET promoted_in = $2 WHERE path = ANY($1)",
                &[&paths, &generation],
            )
            .await?;
        Ok(())
    }

    /// `subject`'s ChatGPT link, if they linked one.
    pub async fn link(&self, subject: &str) -> anyhow::Result<Option<Link>> {
        let row = self
            .db
            .lock()
            .await
            .query_opt(
                "SELECT account, email, client_id, id_token, refresh_token, broken \
                 FROM dirigent.chatgpt_links WHERE subject = $1",
                &[&subject],
            )
            .await?;
        Ok(row.map(|r| Link {
            account: r.get(0),
            email: r.get(1),
            client_id: r.get(2),
            id_token: r.get(3),
            refresh_token: r.get(4),
            broken: r.get(5),
        }))
    }

    /// `subject`'s link is this one now, whatever it was.
    pub async fn put_link(&self, subject: &str, link: &Link) -> anyhow::Result<()> {
        self.db
            .lock()
            .await
            .execute(
                "INSERT INTO dirigent.chatgpt_links \
                 (subject, account, email, client_id, id_token, refresh_token, broken) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7) \
                 ON CONFLICT (subject) DO UPDATE SET account = $2, email = $3, client_id = $4, \
                 id_token = $5, refresh_token = $6, broken = $7, updated = now()",
                &[
                    &subject,
                    &link.account,
                    &link.email,
                    &link.client_id,
                    &link.id_token,
                    &link.refresh_token,
                    &link.broken,
                ],
            )
            .await?;
        Ok(())
    }

    /// The link's refresh token was replaced (`old` by `new`), or it's `broken`: unless it was
    /// linked anew meanwhile (its refresh token isn't `old`); whether it was.
    pub async fn renewed_link(
        &self,
        subject: &str,
        old: &[u8],
        new: Result<&[u8], &str>,
    ) -> anyhow::Result<bool> {
        let db = self.db.lock().await;
        let n = match new {
            Ok(new) => {
                db.execute(
                    "UPDATE dirigent.chatgpt_links SET refresh_token = $3, updated = now() \
                     WHERE subject = $1 AND refresh_token = $2",
                    &[&subject, &old, &new],
                )
                .await?
            }
            Err(broken) => {
                db.execute(
                    "UPDATE dirigent.chatgpt_links SET broken = $3, updated = now() \
                     WHERE subject = $1 AND refresh_token = $2",
                    &[&subject, &old, &broken],
                )
                .await?
            }
        };
        Ok(n == 1)
    }

    /// Whether there was a link.
    pub async fn delete_link(&self, subject: &str) -> anyhow::Result<bool> {
        let n = self
            .db
            .lock()
            .await
            .execute(
                "DELETE FROM dirigent.chatgpt_links WHERE subject = $1",
                &[&subject],
            )
            .await?;
        Ok(n == 1)
    }

    /// `subject`'s API key for `provider`, as it's stored (sealed), and its hint.
    pub async fn api_key(
        &self,
        subject: &str,
        provider: &str,
    ) -> anyhow::Result<Option<(Vec<u8>, String)>> {
        let row = self
            .db
            .lock()
            .await
            .query_opt(
                "SELECT key, hint FROM dirigent.api_keys WHERE subject = $1 AND provider = $2",
                &[&subject, &provider],
            )
            .await?;
        Ok(row.map(|r| (r.get(0), r.get(1))))
    }

    /// `subject`'s API key for `provider` is this one (sealed) now, whatever it was.
    pub async fn put_api_key(
        &self,
        subject: &str,
        provider: &str,
        sealed: &[u8],
        hint: &str,
    ) -> anyhow::Result<()> {
        self.db
            .lock()
            .await
            .execute(
                "INSERT INTO dirigent.api_keys (subject, provider, key, hint) \
                 VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (subject, provider) DO UPDATE SET key = $3, hint = $4, updated = now()",
                &[&subject, &provider, &sealed, &hint],
            )
            .await?;
        Ok(())
    }

    /// Whether there was a key.
    pub async fn delete_api_key(&self, subject: &str, provider: &str) -> anyhow::Result<bool> {
        let n = self
            .db
            .lock()
            .await
            .execute(
                "DELETE FROM dirigent.api_keys WHERE subject = $1 AND provider = $2",
                &[&subject, &provider],
            )
            .await?;
        Ok(n == 1)
    }

    /// The secret `name` of `scope` is this one (sealed) now, whatever it was.
    pub async fn put_secret(
        &self,
        scope: Scope<'_>,
        name: &str,
        sealed: &[u8],
    ) -> anyhow::Result<()> {
        let db = self.db.lock().await;
        match scope {
            Scope::User(subject) => {
                db.execute(
                    "INSERT INTO dirigent.user_secrets (subject, name, value) VALUES ($1, $2, $3) \
                     ON CONFLICT (subject, name) DO UPDATE SET value = $3, updated = now()",
                    &[&subject, &name, &sealed],
                )
                .await?
            }
            Scope::Session(session) => {
                db.execute(
                    "INSERT INTO dirigent.session_secrets (session_id, name, value) \
                     VALUES ($1, $2, $3) \
                     ON CONFLICT (session_id, name) DO UPDATE SET value = $3, updated = now()",
                    &[&session, &name, &sealed],
                )
                .await?
            }
        };
        Ok(())
    }

    /// Whether there was such a secret.
    pub async fn delete_secret(&self, scope: Scope<'_>, name: &str) -> anyhow::Result<bool> {
        let db = self.db.lock().await;
        let n = match scope {
            Scope::User(subject) => {
                db.execute(
                    "DELETE FROM dirigent.user_secrets WHERE subject = $1 AND name = $2",
                    &[&subject, &name],
                )
                .await?
            }
            Scope::Session(session) => {
                db.execute(
                    "DELETE FROM dirigent.session_secrets WHERE session_id = $1 AND name = $2",
                    &[&session, &name],
                )
                .await?
            }
        };
        Ok(n == 1)
    }

    /// The names of `scope`'s secrets, sorted.
    pub async fn secret_names(&self, scope: Scope<'_>) -> anyhow::Result<Vec<String>> {
        let db = self.db.lock().await;
        let rows = match scope {
            Scope::User(subject) => {
                db.query(
                    "SELECT name FROM dirigent.user_secrets WHERE subject = $1 ORDER BY name",
                    &[&subject],
                )
                .await?
            }
            Scope::Session(session) => {
                db.query(
                    "SELECT name FROM dirigent.session_secrets WHERE session_id = $1 ORDER BY name",
                    &[&session],
                )
                .await?
            }
        };
        Ok(rows.iter().map(|r| r.get(0)).collect())
    }

    /// The secrets a session's tools get, sealed, by name: its owner's for all their sessions,
    /// and its own, which win.
    pub async fn session_secrets(&self, session: Uuid) -> anyhow::Result<Vec<(String, Vec<u8>)>> {
        let rows = self
            .db
            .lock()
            .await
            .query(
                "SELECT name, value, 0 AS own FROM dirigent.user_secrets WHERE subject = \
                   (SELECT subject FROM dirigent.session_access \
                    WHERE session_id = $1 AND role = 'owner') \
                 UNION ALL \
                 SELECT name, value, 1 FROM dirigent.session_secrets WHERE session_id = $1 \
                 ORDER BY own",
                &[&session],
            )
            .await?;
        let mut secrets: Vec<(String, Vec<u8>)> = Vec::new();
        for row in rows {
            let name: String = row.get(0);
            secrets.retain(|(n, _)| *n != name);
            secrets.push((name, row.get(1)));
        }
        Ok(secrets)
    }
}

/// Whose secrets: a user's, for all their sessions, or one session's.
#[derive(Debug, Clone, Copy)]
pub enum Scope<'a> {
    User(&'a str),
    Session(Uuid),
}

/// The players' system for a configuration (`dirigent.player_systems`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerSystem {
    /// `building`, `ready`, `failed` or `broken`.
    pub state: String,
    pub build: Uuid,
    pub store_path: Option<String>,
    pub error: Option<String>,
}

/// A user's ChatGPT link (`dirigent.chatgpt_links`), its refresh token as it's stored:
/// encrypted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub account: String,
    pub email: String,
    pub client_id: String,
    pub id_token: String,
    pub refresh_token: Vec<u8>,
    pub broken: Option<String>,
}

/// The sessions the caller (`$1`) owns or views, with their lease holders, owners, viewers and
/// secrets' names, for [`session_info`].
const SESSIONS: &str = "SELECT s.id, s.spec, s.epoch, p.subject, p.expires_at IS NULL, \
     coalesce(o.subject, ''), \
     ARRAY(SELECT v.subject FROM dirigent.session_access v \
           WHERE v.session_id = s.id AND v.role = 'viewer' ORDER BY v.subject), \
     me.role, (extract(epoch FROM s.created) * 1000)::bigint, \
     ARRAY(SELECT name FROM (SELECT name FROM dirigent.session_secrets ss \
             WHERE ss.session_id = s.id \
           UNION SELECT name FROM dirigent.user_secrets us WHERE us.subject = o.subject) n \
           ORDER BY name), \
     (extract(epoch FROM s.archived) * 1000)::bigint \
     FROM dirigent.sessions s \
     JOIN dirigent.session_access me \
       ON me.session_id = s.id AND me.subject = $1 AND me.role IN ('owner', 'viewer') \
     LEFT JOIN dirigent.session_access p ON p.session_id = s.id AND p.role = 'player' \
     LEFT JOIN dirigent.session_access o ON o.session_id = s.id AND o.role = 'owner'";

fn session_info(row: &Row) -> anyhow::Result<SessionInfo> {
    let id: Uuid = row.get(0);
    let subject: Option<String> = row.get(3);
    Ok(SessionInfo {
        id: id.to_string(),
        spec: serde_json::from_value(row.get(1))?,
        epoch: row.get(2),
        player: subject.map(|subject| Holder {
            subject,
            attached: row.get(4),
        }),
        owner: row.get(5),
        viewers: row.get(6),
        role: role(row.get(7)),
        created: row.get(8),
        secrets: row.get(9),
        archived: row.get(10),
    })
}

fn role(r: &str) -> Role {
    match r {
        "owner" => Role::Owner,
        _ => Role::Viewer,
    }
}

/// What a web sign-in's secret is kept as.
fn sha256(secret: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(secret).to_vec()
}
