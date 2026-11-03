//! The player's lease on its session, from the control plane (`dirigent_protocol::control`): held
//! for as long as the WebSocket to control is open. A dropped connection is reconnected and the
//! lease resumed; if control says it's gone, the lease is lost and the player must stop.

use std::time::Duration;

use dirigent_protocol::control::{SessionSpec, ToControl, ToPlayer};
use futures::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How long to wait before asking again for a session that's held or unknown.
const ASK_AGAIN: Duration = Duration::from_secs(1);
/// Control pings every few seconds; this long without a word, the connection counts as dropped.
const SILENT: Duration = Duration::from_secs(15);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

pub struct Lease {
    /// The holder's place in the session's line of players.
    pub epoch: i64,
    /// What the session runs.
    pub spec: SessionSpec,
    lost: CancellationToken,
    release: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

impl Lease {
    /// Hold `session` as `player`, through control's `/player` WebSocket at `url`; waits while
    /// another player holds it or it doesn't exist yet.
    pub async fn acquire(url: &str, session: &str, player: &str) -> Lease {
        let ask = |epoch| ToControl::Acquire {
            session: session.to_string(),
            player: player.to_string(),
            epoch,
        };
        let mut backoff = Backoff::default();
        let (ws, epoch, spec) = loop {
            let mut ws = match tokio_tungstenite::connect_async(url).await {
                Ok((ws, _)) => ws,
                Err(e) => {
                    warn!("connecting to control at {url}: {e}");
                    backoff.wait().await;
                    continue;
                }
            };
            backoff = Backoff::default();
            match hold(&mut ws, &ask(None)).await {
                Ok((epoch, spec)) => break (ws, epoch, spec),
                Err(e) => {
                    warn!("acquiring the session: {e:#}");
                    backoff.wait().await;
                }
            }
        };
        let lost = CancellationToken::new();
        let (release, released) = oneshot::channel();
        let task = tokio::spawn(keep(
            url.to_string(),
            ask(Some(epoch)),
            ws,
            lost.clone(),
            released,
        ));
        Lease {
            epoch,
            spec,
            lost,
            release: Some(release),
            task,
        }
    }

    /// Resolves once the lease is lost.
    pub async fn lost(&self) {
        self.lost.cancelled().await
    }

    /// Give the lease back (closing the connection), so the next player needn't wait.
    pub async fn release(mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut self.task).await;
    }
}

impl Drop for Lease {
    /// Dropped without [`Lease::release`]: the connection drops, so control keeps the lease for
    /// its grace period.
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Asks with `ask` until the session is held; its epoch and spec. `Lost` is an error.
async fn hold(ws: &mut Ws, ask: &ToControl) -> anyhow::Result<(i64, SessionSpec)> {
    let ask = serde_json::to_string(ask).expect("messages serialize");
    let mut waiting = false;
    loop {
        ws.send(Message::text(ask.clone())).await?;
        let reply = loop {
            match tokio::time::timeout(SILENT, ws.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => break serde_json::from_str::<ToPlayer>(&t)?,
                Ok(Some(Ok(Message::Close(_))) | None) => anyhow::bail!("control closed"),
                Ok(Some(Ok(_))) => continue,
                Ok(Some(Err(e))) => return Err(e.into()),
                Err(_) => anyhow::bail!("control went silent"),
            }
        };
        match reply {
            ToPlayer::Acquired { epoch, spec } => return Ok((epoch, spec)),
            ToPlayer::Lost => return Err(LostLease.into()),
            ToPlayer::Held | ToPlayer::UnknownSession => {
                if !waiting {
                    info!("waiting for the session: {reply:?}");
                    waiting = true;
                }
                tokio::time::sleep(ASK_AGAIN).await;
            }
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("the lease is lost")]
struct LostLease;

/// Holds the connection open, reconnecting and resuming (`resume`) when it drops, until the
/// lease is released or lost.
async fn keep(
    url: String,
    resume: ToControl,
    mut ws: Ws,
    lost: CancellationToken,
    mut released: oneshot::Receiver<()>,
) {
    loop {
        // the connection, until it drops
        loop {
            tokio::select! {
                _ = &mut released => {
                    let _ = ws.close(None).await;
                    return;
                }
                msg = tokio::time::timeout(SILENT, ws.next()) => match msg {
                    Ok(Some(Ok(Message::Close(_))) | None | Some(Err(_))) | Err(_) => break,
                    Ok(Some(Ok(_))) => {}
                },
            }
        }
        warn!("lost the connection to control; resuming the lease");
        let mut backoff = Backoff::default();
        ws = loop {
            let mut next = tokio::select! {
                _ = &mut released => return,
                c = tokio_tungstenite::connect_async(&url) => match c {
                    Ok((ws, _)) => ws,
                    Err(e) => {
                        warn!("reconnecting to control: {e}");
                        backoff.wait().await;
                        continue;
                    }
                },
            };
            match hold(&mut next, &resume).await {
                Ok(_) => break next,
                Err(e) if e.is::<LostLease>() => {
                    warn!("control says the lease is gone");
                    lost.cancel();
                    return;
                }
                Err(e) => {
                    warn!("resuming the lease: {e:#}");
                    backoff.wait().await;
                }
            }
        };
        info!("lease resumed");
    }
}

/// Doubling waits, from 100 ms up to [`MAX_BACKOFF`].
struct Backoff(Duration);

impl Default for Backoff {
    fn default() -> Self {
        Backoff(Duration::from_millis(100))
    }
}

impl Backoff {
    async fn wait(&mut self) {
        tokio::time::sleep(self.0).await;
        self.0 = (self.0 * 2).min(MAX_BACKOFF);
    }
}
