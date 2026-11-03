//! dirigent's control plane, the only writer of its Postgres: sessions are made over HTTP
//! (`PUT /sessions/{id}`), and players hold them over a WebSocket each (`/player`, see
//! `dirigent_protocol::control`), the connection being the lease's liveness.

mod leases;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, put};
use axum::{Json, Router};
use clap::Parser;
use dirigent_protocol::control::{player_subject, SessionSpec, ToControl, ToPlayer};
use serde::Deserialize;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::leases::{Acquire, Put, Store};

/// How often a player is pinged, and how long it may be silent before it counts as dropped.
const PING: Duration = Duration::from_secs(5);
const SILENT: Duration = Duration::from_secs(15);

#[derive(Parser, Debug)]
#[command(name = "control", about = "dirigent's control plane")]
struct Cli {
    #[arg(
        long,
        env = "DIRIGENT_CONTROL_LISTEN",
        default_value = "127.0.0.1:8090"
    )]
    listen: SocketAddr,

    /// Postgres, as the `dirigent_control` role.
    #[arg(long, env = "DIRIGENT_CONTROL_POSTGRES")]
    postgres: String,

    /// How long a dropped player keeps its session, in seconds.
    #[arg(long, default_value_t = 10)]
    grace: u64,
}

struct Control {
    store: Store,
    /// The connection holding each lease (session, subject), so that one a player left for a
    /// new one doesn't end the lease when it drops.
    attached: std::sync::Mutex<HashMap<(Uuid, String), u64>>,
    connections: AtomicU64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "control=info".into()),
        )
        .init();
    let cli: &'static Cli = Box::leak(Box::new(Cli::parse()));
    let store = Store::connect(&cli.postgres, Duration::from_secs(cli.grace)).await?;
    let detached = store.detach_all().await?;
    let control = control(store);
    tokio::spawn(sweep(control));
    let listener = tokio::net::TcpListener::bind(cli.listen).await?;
    info!(listen = %cli.listen, grace = cli.grace, detached, "control started");
    tokio::select! {
        r = axum::serve(listener, router(control)) => r?,
        _ = tokio::signal::ctrl_c() => info!("shutting down"),
    }
    Ok(())
}

fn control(store: Store) -> &'static Control {
    Box::leak(Box::new(Control {
        store,
        attached: Default::default(),
        connections: AtomicU64::new(0),
    }))
}

fn router(control: &'static Control) -> Router {
    Router::new()
        .route("/sessions/{id}", put(put_session))
        .route("/player", get(player))
        .with_state(control)
}

/// Deletes the expired leases, every second.
async fn sweep(control: &'static Control) {
    let mut every = tokio::time::interval(Duration::from_secs(1));
    loop {
        every.tick().await;
        match control.store.sweep().await {
            Ok(0) => {}
            Ok(n) => info!(n, "leases expired"),
            Err(e) => error!("sweeping the expired leases: {e:#}"),
        }
    }
}

#[derive(Deserialize)]
struct NewSession {
    /// The subjects with access to it.
    #[serde(default)]
    users: Vec<String>,
    #[serde(flatten)]
    spec: SessionSpec,
}

async fn put_session(
    State(control): State<&'static Control>,
    Path(id): Path<Uuid>,
    Json(body): Json<NewSession>,
) -> Result<StatusCode, (StatusCode, String)> {
    match control.store.put_session(id, &body.spec, &body.users).await {
        Ok(Put::Made) => {
            info!(session = %id, users = ?body.users, spec = ?body.spec, "session made");
            Ok(StatusCode::CREATED)
        }
        Ok(Put::Exists) => Ok(StatusCode::OK),
        Ok(Put::Conflict) => Err((
            StatusCode::CONFLICT,
            "the session exists, with another spec".into(),
        )),
        Err(e) => {
            error!(session = %id, "putting a session: {e:#}");
            Err((StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
        }
    }
}

async fn player(State(control): State<&'static Control>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| attach(control, socket))
}

/// One player's connection: it asks for a session until it holds it, then holds it until it
/// closes the connection (giving it back) or drops (keeping it for the grace period).
async fn attach(control: &'static Control, mut socket: WebSocket) {
    let conn = control.connections.fetch_add(1, Ordering::Relaxed);
    let mut held: Option<(Uuid, String)> = None;
    let mut ping = tokio::time::interval(PING);
    let closed = loop {
        let msg = tokio::select! {
            _ = ping.tick() => {
                if socket.send(Message::Ping(Default::default())).await.is_err() {
                    break false;
                }
                continue;
            }
            msg = tokio::time::timeout(SILENT, socket.recv()) => msg,
        };
        let text = match msg {
            Ok(Some(Ok(Message::Text(text)))) => text,
            Ok(Some(Ok(Message::Close(_)))) => break true,
            Ok(Some(Ok(_))) => continue, // pings and pongs
            Ok(Some(Err(_)) | None) | Err(_) => break false,
        };
        if held.is_some() {
            continue;
        }
        let ToControl::Acquire {
            session,
            player,
            epoch,
        } = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                warn!("a player sent {text:?}: {e}");
                break false;
            }
        };
        let Ok(session) = Uuid::parse_str(&session) else {
            warn!("a player asked for {session:?}, not a session id");
            break false;
        };
        let subject = player_subject(&player);
        let reply = match control.store.acquire(session, &subject, epoch).await {
            Ok(Acquire::Acquired(epoch, spec)) => {
                info!(session = %session, subject, epoch, "lease held");
                held = Some((session, subject.clone()));
                control
                    .attached
                    .lock()
                    .unwrap()
                    .insert((session, subject), conn);
                ToPlayer::Acquired { epoch, spec }
            }
            Ok(Acquire::Held) => ToPlayer::Held,
            Ok(Acquire::Unknown) => ToPlayer::UnknownSession,
            Ok(Acquire::Lost) => {
                info!(session = %session, subject, "lease lost");
                ToPlayer::Lost
            }
            Err(e) => {
                error!(session = %session, subject, "acquiring a lease: {e:#}");
                break false;
            }
        };
        let reply = serde_json::to_string(&reply).expect("messages serialize");
        if socket.send(Message::Text(reply.into())).await.is_err() {
            break false;
        }
    };
    let Some((session, subject)) = held else {
        return;
    };
    {
        let mut attached = control.attached.lock().unwrap();
        let key = (session, subject.clone());
        if attached.get(&key) != Some(&conn) {
            return; // the player is on a newer connection
        }
        attached.remove(&key);
    }
    let r = match closed {
        true => control.store.release(session, &subject).await,
        false => control.store.detach(session, &subject).await,
    };
    match r {
        Ok(()) if closed => info!(session = %session, subject, "lease given back"),
        Ok(()) => info!(session = %session, subject, "player dropped; lease kept for now"),
        Err(e) => error!(session = %session, subject, "ending a lease: {e:#}"),
    }
}

/// Against the running stack's Postgres (`dnvr up`, then `e2e`), with a control of its own and
/// a grace period of a second; skipped unless `DIRIGENT_E2E` is set. Each test has sessions of
/// its own. (Starting control detaches every lease, the stack's too, so that isn't tested here.)
#[cfg(test)]
mod e2e {
    use futures::{SinkExt, StreamExt};
    use tokio::net::TcpStream;
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

    use super::*;

    type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

    /// A control on a port of its own; its base URL.
    async fn start() -> Option<String> {
        std::env::var_os("DIRIGENT_E2E")?;
        let url = std::env::var("DIRIGENT_CONTROL_POSTGRES")
            .expect("DIRIGENT_CONTROL_POSTGRES (the `e2e` script sets it)");
        let store = Store::connect(&url, Duration::from_secs(1)).await.unwrap();
        let control = control(store);
        tokio::spawn(sweep(control));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router(control)).await });
        Some(format!("127.0.0.1:{}", addr.port()))
    }

    async fn put(base: &str, session: Uuid, users: &[&str]) -> reqwest::StatusCode {
        put_json(base, session, serde_json::json!({ "users": users })).await
    }

    async fn put_json(base: &str, session: Uuid, body: serde_json::Value) -> reqwest::StatusCode {
        reqwest::Client::new()
            .put(format!("http://{base}/sessions/{session}"))
            .json(&body)
            .send()
            .await
            .unwrap()
            .status()
    }

    /// What a session put with the default spec acquires.
    fn acquired(epoch: i64) -> ToPlayer {
        ToPlayer::Acquired {
            epoch,
            spec: SessionSpec::default(),
        }
    }

    async fn connect(base: &str) -> Ws {
        tokio_tungstenite::connect_async(format!("ws://{base}/player"))
            .await
            .unwrap()
            .0
    }

    async fn acquire(ws: &mut Ws, session: Uuid, player: &str, epoch: Option<i64>) -> ToPlayer {
        let msg = ToControl::Acquire {
            session: session.to_string(),
            player: player.into(),
            epoch,
        };
        ws.send(WsMessage::text(serde_json::to_string(&msg).unwrap()))
            .await
            .unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(10), ws.next()).await {
                Ok(Some(Ok(WsMessage::Text(t)))) => return serde_json::from_str(&t).unwrap(),
                Ok(Some(Ok(_))) => continue,
                other => panic!("no reply: {other:?}"),
            }
        }
    }

    /// Asks again until the answer isn't `Held`, for at most `within`.
    async fn acquire_within(
        ws: &mut Ws,
        session: Uuid,
        player: &str,
        within: Duration,
    ) -> ToPlayer {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let got = acquire(ws, session, player, None).await;
            if got != ToPlayer::Held || tokio::time::Instant::now() > deadline {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    #[tokio::test]
    async fn e2e_putting_a_session_is_idempotent() {
        let Some(base) = start().await else { return };
        let s = Uuid::new_v4();
        assert_eq!(
            put(&base, s, &["user:a"]).await,
            reqwest::StatusCode::CREATED
        );
        assert_eq!(
            put(&base, s, &["user:a", "user:b"]).await,
            reqwest::StatusCode::OK
        );
        let mut ws = connect(&base).await;
        assert_eq!(
            acquire(&mut ws, Uuid::new_v4(), "p", None).await,
            ToPlayer::UnknownSession
        );
    }

    #[tokio::test]
    async fn e2e_a_session_keeps_the_spec_it_was_made_with() {
        let Some(base) = start().await else { return };
        let s = Uuid::new_v4();
        let spec = serde_json::json!({
            "model": "claude-opus-5",
            "effort": "high",
            "source": { "git": { "url": "https://example.com/r.git", "ref": "dev" } },
        });
        let mut body = spec.clone();
        body["users"] = serde_json::json!(["user:a"]);
        assert_eq!(
            put_json(&base, s, body.clone()).await,
            reqwest::StatusCode::CREATED
        );
        assert_eq!(put_json(&base, s, body).await, reqwest::StatusCode::OK);
        assert_eq!(
            put(&base, s, &["user:b"]).await,
            reqwest::StatusCode::CONFLICT
        );
        let mut ws = connect(&base).await;
        assert_eq!(
            acquire(&mut ws, s, "p", None).await,
            ToPlayer::Acquired {
                epoch: 1,
                spec: serde_json::from_value(spec).unwrap(),
            }
        );
    }

    #[tokio::test]
    async fn e2e_a_held_session_goes_to_the_next_player_once_given_back() {
        let Some(base) = start().await else { return };
        let s = Uuid::new_v4();
        put(&base, s, &[]).await;
        let mut a = connect(&base).await;
        let mut b = connect(&base).await;
        assert_eq!(acquire(&mut a, s, "a", None).await, acquired(1));
        assert_eq!(acquire(&mut b, s, "b", None).await, ToPlayer::Held);
        a.close(None).await.unwrap();
        assert_eq!(
            acquire_within(&mut b, s, "b", Duration::from_secs(5)).await,
            acquired(2)
        );
    }

    #[tokio::test]
    async fn e2e_a_dropped_player_resumes_its_lease_within_the_grace_period() {
        let Some(base) = start().await else { return };
        let s = Uuid::new_v4();
        put(&base, s, &[]).await;
        let mut a = connect(&base).await;
        assert_eq!(acquire(&mut a, s, "a", None).await, acquired(1));
        drop(a); // no Close frame
        let mut b = connect(&base).await;
        assert_eq!(acquire(&mut b, s, "b", None).await, ToPlayer::Held);
        let mut a = connect(&base).await;
        assert_eq!(acquire(&mut a, s, "a", Some(1)).await, acquired(1));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(acquire(&mut b, s, "b", None).await, ToPlayer::Held);
    }

    #[tokio::test]
    async fn e2e_past_the_grace_period_the_lease_goes_to_another_player() {
        let Some(base) = start().await else { return };
        let s = Uuid::new_v4();
        put(&base, s, &[]).await;
        let mut a = connect(&base).await;
        assert_eq!(acquire(&mut a, s, "a", None).await, acquired(1));
        drop(a);
        let mut b = connect(&base).await;
        assert_eq!(
            acquire_within(&mut b, s, "b", Duration::from_secs(5)).await,
            acquired(2)
        );
        let mut a = connect(&base).await;
        assert_eq!(acquire(&mut a, s, "a", Some(1)).await, ToPlayer::Lost);
    }
}
