//! The player's lease on its session, from the control plane: a gRPC call (`Players.Lease`, see
//! `protocol/proto`), held for as long as the call is open. A broken call is made again and the
//! lease resumed; if control says it's gone, the lease is lost and the player must stop. Once the
//! session is stopped, control tells the player to detach ([`Lease::detach`]).

use std::time::Duration;

use dirigent_protocol::control::pb::players_client::PlayersClient;
use dirigent_protocol::control::{pb, SessionSpec};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::Endpoint;
use tonic::{Code, Streaming};
use tracing::{info, warn};

/// The connection to control is pinged this often, and counts as broken when a ping goes
/// unanswered for `KEEPALIVE_TIMEOUT` (as does control, the other way).
const KEEPALIVE: Duration = Duration::from_secs(5);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long control may take to answer an `Acquire`.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BACKOFF: Duration = Duration::from_secs(5);

pub struct Lease {
    /// The holder's place in the session's line of players.
    pub epoch: i64,
    /// What the session runs.
    pub spec: SessionSpec,
    lost: CancellationToken,
    release: Option<oneshot::Sender<()>>,
    detach: watch::Receiver<Option<bool>>,
    task: JoinHandle<()>,
}

/// A held lease's call: the player's stream to control, and control's to it.
struct Call {
    tx: mpsc::Sender<pb::FromPlayer>,
    rx: Streaming<pb::ToPlayer>,
}

impl Lease {
    /// Hold `session` as `player`, through control's gRPC service at `url`: the lease `epoch`
    /// control reserved for it, or the session if it's free. Retries while control can't be
    /// reached; another player holding the session, or the reserved lease gone, is an error.
    pub async fn acquire(
        url: &str,
        session: &str,
        player: &str,
        epoch: Option<i64>,
    ) -> anyhow::Result<Lease> {
        let ask = |epoch| pb::Acquire {
            session: session.to_string(),
            player: player.to_string(),
            epoch,
        };
        let mut backoff = Backoff::default();
        let (call, epoch, spec) = loop {
            match hold(url, ask(epoch)).await {
                Ok(held) => break held,
                Err(e) if e.is::<Refused>() => return Err(e),
                Err(e) => {
                    warn!("acquiring the session from control at {url}: {e:#}");
                    backoff.wait().await;
                }
            }
        };
        let lost = CancellationToken::new();
        let (release, released) = oneshot::channel();
        let (detach, detached) = watch::channel(None);
        let task = tokio::spawn(keep(
            url.to_string(),
            ask(Some(epoch)),
            call,
            lost.clone(),
            released,
            detach,
        ));
        Ok(Lease {
            epoch,
            spec,
            lost,
            release: Some(release),
            detach: detached,
            task,
        })
    }

    /// Whether to stop now, once control says the session is stopped; `None` until then. A
    /// graceful stop may turn into one that's now.
    pub fn detach(&self) -> watch::Receiver<Option<bool>> {
        self.detach.clone()
    }

    /// Resolves once the lease is lost.
    pub async fn lost(&self) {
        self.lost.cancelled().await
    }

    /// Give the lease back (ending the call's stream), so the next player needn't wait.
    pub async fn release(mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(());
        }
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut self.task).await;
    }
}

impl Drop for Lease {
    /// Dropped without [`Lease::release`]: the call breaks, so control keeps the lease for its
    /// grace period.
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A call to control at `url` asking for the session with `ask`: the call, with the lease's
/// epoch and the spec; or why not ([`Refused`], or not reaching control).
async fn hold(url: &str, ask: pb::Acquire) -> anyhow::Result<(Call, i64, SessionSpec)> {
    let channel = Endpoint::from_shared(url.to_string())?
        .connect_timeout(CONNECT_TIMEOUT)
        .http2_keep_alive_interval(KEEPALIVE)
        .keep_alive_timeout(KEEPALIVE_TIMEOUT)
        .keep_alive_while_idle(true)
        .connect()
        .await?;
    let (tx, rx) = mpsc::channel(4);
    let ask = pb::FromPlayer {
        msg: Some(pb::from_player::Msg::Acquire(ask)),
    };
    tx.send(ask).await.expect("the receiver is here");
    let answer = async {
        let mut rx = PlayersClient::new(channel)
            .lease(ReceiverStream::new(rx))
            .await
            .map_err(refused)?
            .into_inner();
        match rx.message().await.map_err(refused)? {
            Some(pb::ToPlayer {
                msg: Some(pb::to_player::Msg::Acquired(a)),
            }) => {
                let spec = a.spec.unwrap_or_default().try_into();
                let spec = spec.map_err(|e| anyhow::anyhow!("control sent {e}"))?;
                Ok((rx, a.epoch, spec))
            }
            Some(other) => anyhow::bail!("control answered {other:?}, not Acquired"),
            None => anyhow::bail!("control ended the call"),
        }
    };
    let (rx, epoch, spec) = tokio::time::timeout(ANSWER_TIMEOUT, answer)
        .await
        .map_err(|_| anyhow::anyhow!("control didn't answer"))??;
    Ok((Call { tx, rx }, epoch, spec))
}

/// A status ending the call: control's refusal, or another error.
fn refused(status: tonic::Status) -> anyhow::Error {
    match status.code() {
        Code::FailedPrecondition => Refused::Held.into(),
        Code::NotFound => Refused::Unknown.into(),
        Code::Aborted => Refused::Lost.into(),
        _ => status.into(),
    }
}

/// Why control won't lease the session: final, unlike not reaching it.
#[derive(Debug, thiserror::Error)]
pub enum Refused {
    #[error("another player holds the session")]
    Held,
    #[error("there's no such session")]
    Unknown,
    #[error("the lease is lost")]
    Lost,
}

/// Holds the call open, calling again and resuming (`resume`) when it breaks, until the lease
/// is released or lost; passes control's word to detach on to `detach`.
async fn keep(
    url: String,
    resume: pb::Acquire,
    mut call: Call,
    lost: CancellationToken,
    mut released: oneshot::Receiver<()>,
    detach: watch::Sender<Option<bool>>,
) {
    loop {
        // the call, until it breaks
        loop {
            tokio::select! {
                _ = &mut released => {
                    // the stream ends: control gives the lease back and ends the call
                    let Call { tx, mut rx } = call;
                    drop(tx);
                    while let Ok(Some(_)) = rx.message().await {}
                    return;
                }
                msg = call.rx.message() => match msg {
                    Ok(Some(pb::ToPlayer { msg: Some(pb::to_player::Msg::Detach(d)) })) => {
                        info!(now = d.now, "control says detach");
                        // a stop that's now stays so
                        detach.send_modify(|v| *v = Some(d.now || v.unwrap_or(false)));
                    }
                    Ok(Some(other)) => warn!("control sent {other:?}"),
                    Ok(None) | Err(_) => break,
                },
            }
        }
        warn!("lost the call to control; resuming the lease");
        let mut backoff = Backoff::default();
        call = loop {
            let held = tokio::select! {
                _ = &mut released => return,
                held = hold(&url, resume.clone()) => held,
            };
            match held {
                Ok((call, _, _)) => break call,
                Err(e) if e.is::<Refused>() => {
                    warn!("control says the lease is gone: {e}");
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
