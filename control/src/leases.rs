//! The control plane's state, in Postgres (`db/postgres`): the sessions, and who may use each
//! (`session_access`, which chlive grants from), its users and the one player holding its lease.
//! A lease row's `expires_at` is null while its player is connected; once it isn't, the player
//! may come back for it until then, and past it the row is deleted ([`Store::sweep`]).

use std::time::Duration;

use anyhow::Context;
use dirigent_protocol::control::SessionSpec;
use tokio::sync::Mutex;
use tokio_postgres::{Client, NoTls};
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

    /// Delete the leases that expired (chlive's grants go with them); how many.
    pub async fn sweep(&self) -> anyhow::Result<u64> {
        Ok(self
            .db
            .lock()
            .await
            .execute(
                "DELETE FROM dirigent.session_access WHERE player AND expires_at < now()",
                &[],
            )
            .await?)
    }
}
