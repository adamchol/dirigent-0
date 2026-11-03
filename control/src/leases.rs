//! The control plane's state, in Postgres (`db/postgres`): the sessions, and who may use each
//! (`session_access`, which chlive grants from), its users and the one player holding its lease.
//! A lease row's `expires_at` is null while its player is connected; once it isn't, the player
//! may come back for it until then, and past it the row is deleted ([`Store::sweep`]). A session
//! with a lease is active; one without, idle. Control starts a player by reserving the lease for
//! it ([`Store::reserve`]), which the player then takes as its own.

use std::time::Duration;

use anyhow::Context;
use dirigent_protocol::control::{Holder, SessionInfo, SessionSpec};
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
}

/// What a player asking for a session gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquire {
    /// It holds it, as the `epoch`th player, and runs the spec.
    Acquired(i64, SessionSpec),
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

    /// The session `id`, made with `spec` if there's none, with access for `users` (subjects)
    /// besides those it has. One that's there keeps its spec: given another, nothing changes.
    pub async fn put_session(
        &self,
        id: Uuid,
        spec: &SessionSpec,
        users: &[String],
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
            Put::Made
        } else {
            let row = tx
                .query_one(
                    "SELECT spec FROM dirigent.sessions WHERE id = $1 FOR UPDATE",
                    &[&id],
                )
                .await?;
            let had: SessionSpec = serde_json::from_value(row.get(0))?;
            if had != *spec {
                return Ok(Put::Conflict);
            }
            Put::Exists
        };
        for user in users {
            tx.execute(
                "INSERT INTO dirigent.session_access (session_id, subject) VALUES ($1, $2) \
                 ON CONFLICT DO NOTHING",
                &[&id, user],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(put)
    }

    /// The session `id`, if there's one.
    pub async fn session(&self, id: Uuid) -> anyhow::Result<Option<SessionInfo>> {
        let rows = self
            .db
            .lock()
            .await
            .query(&format!("{SESSIONS} WHERE s.id = $1"), &[&id])
            .await?;
        rows.iter().map(session_info).next().transpose()
    }

    pub async fn sessions(&self) -> anyhow::Result<Vec<SessionInfo>> {
        let rows = self
            .db
            .lock()
            .await
            .query(&format!("{SESSIONS} ORDER BY s.id"), &[])
            .await?;
        rows.iter().map(session_info).collect()
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
        let holder = tx
            .query_opt(
                "SELECT subject, expires_at IS NULL OR expires_at > now() \
                 FROM dirigent.session_access WHERE session_id = $1 AND player",
                &[&session],
            )
            .await?;
        let got = match holder {
            Some(h) if h.get::<_, &str>(0) == subject => {
                tx.execute(
                    "UPDATE dirigent.session_access SET expires_at = NULL \
                     WHERE session_id = $1 AND player",
                    &[&session],
                )
                .await?;
                Acquire::Acquired(epoch, spec)
            }
            _ if resume.is_some() => Acquire::Lost,
            Some(h) if h.get::<_, bool>(1) => Acquire::Held,
            _ => {
                tx.execute(
                    "DELETE FROM dirigent.session_access WHERE session_id = $1 AND player",
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
                    "INSERT INTO dirigent.session_access (session_id, subject, player) \
                     VALUES ($1, $2, true)",
                    &[&session, &subject],
                )
                .await?;
                Acquire::Acquired(epoch, spec)
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
                 WHERE session_id = $1 AND subject = $2 AND player",
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
                 WHERE session_id = $1 AND subject = $2 AND player AND expires_at IS NULL",
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
                 WHERE player AND expires_at IS NULL",
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
                "DELETE FROM dirigent.session_access WHERE player AND expires_at < now() \
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
                 FROM dirigent.session_access WHERE session_id = $1 AND player",
                &[&session],
            )
            .await?;
        match held.map(|r| r.get::<_, bool>(0)) {
            Some(true) => return Ok(Reserve::Held),
            Some(false) => {
                tx.execute(
                    "DELETE FROM dirigent.session_access WHERE session_id = $1 AND player",
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
             (session_id, subject, player, spawned_by, expires_at) \
             VALUES ($1, $2, true, $3, now() + make_interval(secs => $4))",
            &[&session, &subject, &runner, &startup.as_secs_f64()],
        )
        .await?;
        tx.commit().await?;
        Ok(Reserve::Reserved(epoch))
    }

    /// Delete the lease on `session`, whoever holds it; whether there was one.
    pub async fn drop_lease(&self, session: Uuid) -> anyhow::Result<bool> {
        let n = self
            .db
            .lock()
            .await
            .execute(
                "DELETE FROM dirigent.session_access WHERE session_id = $1 AND player",
                &[&session],
            )
            .await?;
        Ok(n == 1)
    }

    /// Whether there's a session `id`.
    pub async fn exists(&self, id: Uuid) -> anyhow::Result<bool> {
        let row = self
            .db
            .lock()
            .await
            .query_opt("SELECT 1 FROM dirigent.sessions WHERE id = $1", &[&id])
            .await?;
        Ok(row.is_some())
    }
}

/// The sessions with their lease holders, for [`session_info`].
const SESSIONS: &str = "SELECT s.id, s.spec, s.epoch, a.subject, a.expires_at IS NULL \
     FROM dirigent.sessions s \
     LEFT JOIN dirigent.session_access a ON a.session_id = s.id AND a.player";

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
    })
}
