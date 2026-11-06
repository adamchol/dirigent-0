//! The control plane's state, in Postgres (`db/postgres`): the sessions, and who may use each
//! (`session_access`, which chlive grants from): its owner, its viewers, and the one player
//! holding its lease.
//! A lease row's `expires_at` is null while its player is connected; once it isn't, the player
//! may come back for it until then, and past it the row is deleted ([`Store::sweep`]). A session
//! with a lease is active; one without, idle. Control starts a player by reserving the lease for
//! it ([`Store::reserve`]), which the player then takes as its own.

use std::time::Duration;

use anyhow::Context;
use dirigent_protocol::control::{Holder, Role, SessionInfo, SessionSpec};
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
}

/// What reserving a session's lease for a player control starts did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reserve {
    /// For the `epoch`th player.
    Reserved(i64),
    /// A player holds it already.
    Held,
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
                "SELECT epoch, spec FROM dirigent.sessions WHERE id = $1 FOR UPDATE",
                &[&session],
            )
            .await?
        else {
            return Ok(match resume {
                Some(_) => Acquire::Lost,
                None => Acquire::Unknown,
            });
        };
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
                "SELECT 1 FROM dirigent.sessions WHERE id = $1 FOR UPDATE",
                &[&session],
            )
            .await?;
        if found.is_none() {
            return Ok(Reserve::Unknown);
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
}

/// The sessions the caller (`$1`) owns or views, with their lease holders, owners and viewers,
/// for [`session_info`].
const SESSIONS: &str = "SELECT s.id, s.spec, s.epoch, p.subject, p.expires_at IS NULL, \
     coalesce(o.subject, ''), \
     ARRAY(SELECT v.subject FROM dirigent.session_access v \
           WHERE v.session_id = s.id AND v.role = 'viewer' ORDER BY v.subject), \
     me.role \
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
    })
}

fn role(r: &str) -> Role {
    match r {
        "owner" => Role::Owner,
        _ => Role::Viewer,
    }
}
