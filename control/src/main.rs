//! dirigent's control plane, the only writer of its Postgres. Sessions are made, started,
//! stopped and read over HTTP (`/sessions`); players hold them over gRPC (`Players.Lease`, see
//! `protocol/proto`), each call being a lease, its connection's keepalives its liveness. A session with
//! a lease is active, one without idle: starting one, control reserves the lease and starts a
//! player for it (`runner`); stopping one, it tells the player to detach. A player it started
//! that's lost without giving the lease back is started again once the lease expires.

mod leases;
mod runner;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use dirigent_protocol::control::pb::players_server::{Players, PlayersServer};
use dirigent_protocol::control::{pb, player_subject, SessionInfo, SessionSpec};
use serde::Deserialize;
use tokio::sync::{mpsc, watch, Mutex};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status, Streaming};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::leases::{Acquire, Put, Reserve, Store};
use crate::runner::{Local, Runner};

/// How often a player's connection is pinged, and how long it may go unanswered before the
/// connection (and its lease's call) counts as broken.
const KEEPALIVE: Duration = Duration::from_secs(5);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a player has to ask for its session once its call is open.
const ASK_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Parser, Debug)]
#[command(name = "control", about = "dirigent's control plane")]
struct Cli {
    #[arg(
        long,
        env = "DIRIGENT_CONTROL_LISTEN",
        default_value = "127.0.0.1:8090"
    )]
    listen: SocketAddr,

    /// Where players hold their sessions (gRPC).
    #[arg(
        long,
        env = "DIRIGENT_CONTROL_GRPC_LISTEN",
        default_value = "127.0.0.1:8091"
    )]
    grpc_listen: SocketAddr,

    /// Postgres, as the `dirigent_control` role.
    #[arg(long, env = "DIRIGENT_CONTROL_POSTGRES")]
    postgres: String,

    /// How long a dropped player keeps its session, in seconds.
    #[arg(long, default_value_t = 10)]
    grace: u64,

    /// Run players as local processes, each session's in `<players>/<session>`. Without it,
    /// sessions can't be started.
    #[arg(long, env = "DIRIGENT_PLAYERS")]
    players: Option<PathBuf>,

    /// The player program.
    #[arg(long, default_value = "player")]
    player: PathBuf,

    /// How long a player control started has to take its lease, in seconds.
    #[arg(long, default_value_t = 60)]
    startup: u64,

    /// More arguments for each player (e.g. `-- --mock`).
    #[arg(last = true)]
    player_args: Vec<String>,
}

struct Control {
    store: Store,
    runner: Option<&'static dyn Runner>,
    startup: Duration,
    /// The connection holding each session's lease. Held across taking a lease and noting its
    /// connection here, and across a stop's looking here and dropping the lease: a stop either
    /// finds the connection or drops a lease no one is connected for.
    attached: Mutex<HashMap<Uuid, Attached>>,
    connections: AtomicU64,
}

struct Attached {
    /// Which connection, so that one a player left for a new one doesn't end the lease when it
    /// drops.
    conn: u64,
    /// Set when the session is stopped: whether its player should stop now.
    detach: watch::Sender<Option<bool>>,
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
    let runner: Option<&'static dyn Runner> = match &cli.players {
        Some(root) => {
            std::fs::create_dir_all(root)?;
            let root = root.canonicalize()?;
            Some(Box::leak(Box::new(Local {
                id: format!("local:{}", root.display()),
                root,
                program: cli.player.clone(),
                args: cli.player_args.clone(),
                control_url: format!("http://{}", cli.grpc_listen),
            })))
        }
        None => None,
    };
    let control = control(store, runner, Duration::from_secs(cli.startup));
    tokio::spawn(sweep(control));
    let listener = tokio::net::TcpListener::bind(cli.listen).await?;
    let grpc = tokio::net::TcpListener::bind(cli.grpc_listen).await?;
    info!(
        listen = %cli.listen,
        grpc = %cli.grpc_listen,
        grace = cli.grace,
        runner = runner.map(|r| r.id()),
        detached,
        "control started"
    );
    tokio::select! {
        r = axum::serve(listener, router(control)) => r?,
        r = players(control).serve_with_incoming(TcpListenerStream::new(grpc)) => r?,
        _ = tokio::signal::ctrl_c() => info!("shutting down"),
    }
    Ok(())
}

fn control(
    store: Store,
    runner: Option<&'static dyn Runner>,
    startup: Duration,
) -> &'static Control {
    Box::leak(Box::new(Control {
        store,
        runner,
        startup,
        attached: Default::default(),
        connections: AtomicU64::new(0),
    }))
}

fn router(control: &'static Control) -> Router {
    Router::new()
        .route("/sessions", get(list_sessions))
        .route("/sessions/{id}", get(get_session).put(put_session))
        .route("/sessions/{id}/start", post(start_session))
        .route("/sessions/{id}/stop", post(stop_session))
        .with_state(control)
}

/// Deletes the expired leases, every second, and starts another player for each that one of
/// ours held: it was lost (crashed, or never came) without giving the session back.
async fn sweep(control: &'static Control) {
    let runner = control.runner.map_or("", |r| r.id());
    let mut every = tokio::time::interval(Duration::from_secs(1));
    loop {
        every.tick().await;
        let expired = match control.store.sweep(runner).await {
            Ok(expired) => expired,
            Err(e) => {
                error!("sweeping the expired leases: {e:#}");
                continue;
            }
        };
        for (session, ours) in expired {
            if !ours {
                info!(session = %session, "lease expired");
                continue;
            }
            warn!(session = %session, "player lost; starting another");
            if let Err(e) = start(control, session).await {
                error!(session = %session, "starting a player: {e:#}");
            }
        }
    }
}

/// What starting a session did.
#[derive(Debug, PartialEq, Eq)]
enum Started {
    /// A player for it is on its way, as the `epoch`th.
    Player(i64),
    /// It's active already.
    Active,
    Unknown,
    /// There's no runner to start players with.
    NoRunner,
}

/// Start a player for `session`, unless one holds it.
async fn start(control: &'static Control, session: Uuid) -> anyhow::Result<Started> {
    let Some(runner) = control.runner else {
        return Ok(Started::NoRunner);
    };
    let player = Uuid::new_v4().to_string();
    let subject = player_subject(&player);
    let epoch = match control
        .store
        .reserve(session, &subject, runner.id(), control.startup)
        .await?
    {
        Reserve::Reserved(epoch) => epoch,
        Reserve::Held => return Ok(Started::Active),
        Reserve::Unknown => return Ok(Started::Unknown),
    };
    if let Err(e) = runner.start(session, &player, epoch) {
        control.store.drop_lease(session).await?;
        return Err(e);
    }
    Ok(Started::Player(epoch))
}

#[derive(Deserialize)]
struct NewSession {
    /// The subjects with access to it.
    #[serde(default)]
    users: Vec<String>,
    #[serde(flatten)]
    spec: SessionSpec,
    /// Start it too, as `POST …/start` does.
    #[serde(default)]
    start: bool,
}

type Failed = (StatusCode, String);

fn failed(what: &str, e: anyhow::Error) -> Failed {
    error!("{what}: {e:#}");
    (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
}

fn not_found() -> Failed {
    (StatusCode::NOT_FOUND, "no such session".into())
}

async fn put_session(
    State(control): State<&'static Control>,
    Path(id): Path<Uuid>,
    Json(body): Json<NewSession>,
) -> Result<StatusCode, Failed> {
    let status = match control.store.put_session(id, &body.spec, &body.users).await {
        Ok(Put::Made) => {
            info!(session = %id, users = ?body.users, spec = ?body.spec, "session made");
            StatusCode::CREATED
        }
        Ok(Put::Exists) => StatusCode::OK,
        Ok(Put::Conflict) => {
            return Err((
                StatusCode::CONFLICT,
                "the session exists, with another spec".into(),
            ))
        }
        Err(e) => return Err(failed(&format!("putting the session {id}"), e)),
    };
    if body.start {
        started(id, start(control, id).await)?;
    }
    Ok(status)
}

/// A start's outcome, as the API answers it.
fn started(id: Uuid, r: anyhow::Result<Started>) -> Result<(), Failed> {
    match r {
        Ok(Started::Player(epoch)) => {
            info!(session = %id, epoch, "session started");
            Ok(())
        }
        Ok(Started::Active) => Ok(()),
        Ok(Started::Unknown) => Err(not_found()),
        Ok(Started::NoRunner) => Err((
            StatusCode::NOT_IMPLEMENTED,
            "control runs no players (no --players)".into(),
        )),
        Err(e) => Err(failed(&format!("starting the session {id}"), e)),
    }
}

async fn list_sessions(
    State(control): State<&'static Control>,
) -> Result<Json<Vec<SessionInfo>>, Failed> {
    match control.store.sessions().await {
        Ok(sessions) => Ok(Json(sessions)),
        Err(e) => Err(failed("listing the sessions", e)),
    }
}

async fn get_session(
    State(control): State<&'static Control>,
    Path(id): Path<Uuid>,
) -> Result<Json<SessionInfo>, Failed> {
    match control.store.session(id).await {
        Ok(Some(info)) => Ok(Json(info)),
        Ok(None) => Err(not_found()),
        Err(e) => Err(failed(&format!("reading the session {id}"), e)),
    }
}

/// Start (or resume) the session: a player for it, unless it's active.
async fn start_session(
    State(control): State<&'static Control>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Failed> {
    started(id, start(control, id).await)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
struct Stop {
    /// Cancel the turn running instead of letting it finish.
    #[serde(default)]
    now: bool,
}

/// Stop the session: its player, connected, is told to detach; one that isn't (away, or not
/// come yet) loses the lease.
async fn stop_session(
    State(control): State<&'static Control>,
    Path(id): Path<Uuid>,
    body: Option<Json<Stop>>,
) -> Result<StatusCode, Failed> {
    let now = body.map(|b| b.0).unwrap_or_default().now;
    let attached = control.attached.lock().await;
    if let Some(a) = attached.get(&id) {
        // a stop that's now stays so
        a.detach
            .send_modify(|d| *d = Some(now || d.unwrap_or(false)));
        info!(session = %id, now, "session stopping");
        return Ok(StatusCode::NO_CONTENT);
    }
    let dropped = control.store.drop_lease(id).await;
    drop(attached);
    match dropped {
        Ok(true) => {
            info!(session = %id, "session stopped; its player wasn't connected");
            Ok(StatusCode::NO_CONTENT)
        }
        Ok(false) => match control.store.exists(id).await {
            Ok(true) => Ok(StatusCode::NO_CONTENT),
            Ok(false) => Err(not_found()),
            Err(e) => Err(failed(&format!("stopping the session {id}"), e)),
        },
        Err(e) => Err(failed(&format!("stopping the session {id}"), e)),
    }
}

/// The players' gRPC service (`Players`, see `protocol/proto`): HTTP/2, with keepalives as the
/// connections' liveness.
fn players(control: &'static Control) -> tonic::transport::server::Router {
    tonic::transport::Server::builder()
        .http2_keepalive_interval(Some(KEEPALIVE))
        .http2_keepalive_timeout(Some(KEEPALIVE_TIMEOUT))
        .add_service(PlayersServer::new(Lessor(control)))
}

struct Lessor(&'static Control);

#[tonic::async_trait]
impl Players for Lessor {
    type LeaseStream = ReceiverStream<Result<pb::ToPlayer, Status>>;

    /// A player asks for a session; held, it keeps it until it ends its stream (giving it back)
    /// or the call breaks (keeping it for the grace period). Once the session is stopped, the
    /// player is told to detach.
    async fn lease(
        &self,
        request: Request<Streaming<pb::FromPlayer>>,
    ) -> Result<Response<Self::LeaseStream>, Status> {
        let control = self.0;
        let mut inbound = request.into_inner();
        let first = tokio::time::timeout(ASK_TIMEOUT, inbound.message())
            .await
            .map_err(|_| Status::deadline_exceeded("no Acquire"))??;
        let Some(pb::FromPlayer {
            msg: Some(pb::from_player::Msg::Acquire(ask)),
        }) = first
        else {
            return Err(Status::invalid_argument("the first message is an Acquire"));
        };
        let session = Uuid::parse_str(&ask.session).map_err(|_| {
            Status::invalid_argument(format!("{:?} isn't a session id", ask.session))
        })?;
        let subject = player_subject(&ask.player);
        let conn = control.connections.fetch_add(1, Ordering::Relaxed);
        let mut attached = control.attached.lock().await;
        let (epoch, spec) = match control.store.acquire(session, &subject, ask.epoch).await {
            Ok(Acquire::Acquired(epoch, spec)) => (epoch, spec),
            Ok(Acquire::Held) => {
                return Err(Status::failed_precondition(
                    "another player holds the session",
                ))
            }
            Ok(Acquire::Unknown) => return Err(Status::not_found("no such session")),
            Ok(Acquire::Lost) => {
                info!(session = %session, subject, "lease lost");
                return Err(Status::aborted("the lease is gone"));
            }
            Err(e) => {
                error!(session = %session, subject, "acquiring a lease: {e:#}");
                return Err(Status::internal(format!("{e:#}")));
            }
        };
        info!(session = %session, subject, epoch, "lease held");
        let (detach, detached) = watch::channel(None);
        attached.insert(session, Attached { conn, detach });
        drop(attached);
        let (tx, rx) = mpsc::channel(4);
        let acquired = pb::ToPlayer {
            msg: Some(pb::to_player::Msg::Acquired(pb::Acquired {
                epoch,
                spec: Some(spec.into()),
            })),
        };
        tx.send(Ok(acquired)).await.expect("the receiver is here");
        tokio::spawn(hold(control, session, subject, conn, inbound, tx, detached));
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

/// A held lease, until its call ends: then given back (the player ended its stream) or kept
/// for the grace period (the call broke).
async fn hold(
    control: &'static Control,
    session: Uuid,
    subject: String,
    conn: u64,
    mut inbound: Streaming<pb::FromPlayer>,
    tx: mpsc::Sender<Result<pb::ToPlayer, Status>>,
    mut detached: watch::Receiver<Option<bool>>,
) {
    let closed = loop {
        tokio::select! {
            msg = inbound.message() => match msg {
                Ok(Some(_)) => continue, // nothing more to ask once held
                Ok(None) => break true,
                Err(_) => break false,
            },
            Ok(()) = detached.changed() => {
                let Some(now) = *detached.borrow_and_update() else { continue };
                let detach = pb::ToPlayer {
                    msg: Some(pb::to_player::Msg::Detach(pb::Detach { now })),
                };
                if tx.send(Ok(detach)).await.is_err() {
                    break false;
                }
            }
            _ = tx.closed() => break false,
        }
    };
    {
        let mut attached = control.attached.lock().await;
        if attached.get(&session).map(|a| a.conn) != Some(conn) {
            return; // the player is on a newer connection
        }
        attached.remove(&session);
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

/// Against the running stack's Postgres (`dnvr up`, then `e2e`), with a control of its own, a
/// grace period and startup time of a second, and a runner that only notes the players it's to
/// start (or the real one, with mock players); skipped unless `DIRIGENT_E2E` is set. Each test
/// has sessions of its own. (Starting control detaches every lease, the stack's too, so that
/// isn't tested here.)
#[cfg(test)]
mod e2e {
    use dirigent_protocol::control::pb::players_client::PlayersClient;
    use tonic::Code;

    use super::*;

    /// A control of the tests': its HTTP API's address (what it shows as) and its gRPC one.
    struct Ctl {
        http: String,
        grpc: String,
    }

    impl std::fmt::Display for Ctl {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.http)
        }
    }

    /// What control answers a player, as the tests compare it.
    #[derive(Debug, PartialEq)]
    enum Reply {
        Acquired {
            epoch: i64,
            spec: SessionSpec,
        },
        Detach {
            now: bool,
        },
        /// The call ended with this status.
        Refused(Code),
    }

    const HELD: Reply = Reply::Refused(Code::FailedPrecondition);
    const UNKNOWN: Reply = Reply::Refused(Code::NotFound);
    const LOST: Reply = Reply::Refused(Code::Aborted);

    /// A player's connection to control: its lease's call, once it holds one, through a proxy
    /// of its own. Dropped, the connection is cut without the call's stream ending, as a crashed
    /// player's would be; [`Conn::close`] ends it cleanly.
    struct Conn {
        grpc: String,
        call: Option<Call>,
    }

    struct Call {
        tx: tokio::sync::mpsc::Sender<pb::FromPlayer>,
        rx: Streaming<pb::ToPlayer>,
        proxy: tokio::task::JoinHandle<()>,
    }

    /// A proxy to `to` for one connection; its address, and the task to abort to cut it.
    async fn proxy(to: &str) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let to = to.to_string();
        let task = tokio::spawn(async move {
            let (mut client, _) = listener.accept().await.unwrap();
            let mut server = tokio::net::TcpStream::connect(&to).await.unwrap();
            let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
        });
        (addr, task)
    }

    impl Conn {
        /// End the stream (giving the lease back) and wait for control to end the call.
        async fn close(mut self) {
            let Call { tx, mut rx, proxy } = self.call.take().expect("a lease");
            drop(tx);
            let end = tokio::time::timeout(Duration::from_secs(10), async {
                while let Ok(Some(_)) = rx.message().await {}
            });
            end.await.expect("control didn't end the call");
            proxy.abort();
        }
    }

    impl Drop for Conn {
        fn drop(&mut self) {
            if let Some(Call { tx, rx, proxy }) = self.call.take() {
                // the connection goes first, with the stream still open
                proxy.abort();
                std::mem::forget(tx);
                drop(rx);
            }
        }
    }

    fn reply(msg: pb::ToPlayer) -> Reply {
        match msg.msg.expect("a message") {
            pb::to_player::Msg::Acquired(a) => Reply::Acquired {
                epoch: a.epoch,
                spec: a.spec.expect("a spec").try_into().unwrap(),
            },
            pb::to_player::Msg::Detach(d) => Reply::Detach { now: d.now },
        }
    }

    async fn connect(ctl: &Ctl) -> Conn {
        Conn {
            grpc: ctl.grpc.clone(),
            call: None,
        }
    }

    /// Ask for `session` as `player` in a new call; held, the call stays `conn`'s.
    async fn acquire(conn: &mut Conn, session: Uuid, player: &str, epoch: Option<i64>) -> Reply {
        let (via, proxy) = proxy(&conn.grpc).await;
        let mut client = PlayersClient::connect(format!("http://{via}"))
            .await
            .unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let ask = pb::FromPlayer {
            msg: Some(pb::from_player::Msg::Acquire(pb::Acquire {
                session: session.to_string(),
                player: player.into(),
                epoch,
            })),
        };
        tx.send(ask).await.unwrap();
        let call = tokio::time::timeout(
            Duration::from_secs(10),
            client.lease(tokio_stream::wrappers::ReceiverStream::new(rx)),
        )
        .await
        .expect("no reply");
        let mut inbound = match call {
            Ok(r) => r.into_inner(),
            Err(status) => return Reply::Refused(status.code()),
        };
        let got = match tokio::time::timeout(Duration::from_secs(10), inbound.message()).await {
            Ok(Ok(Some(msg))) => reply(msg),
            Ok(Err(status)) => return Reply::Refused(status.code()),
            other => panic!("no reply: {other:?}"),
        };
        conn.call = Some(Call {
            tx,
            rx: inbound,
            proxy,
        });
        got
    }

    /// The next message on the held lease's call, within 10 s.
    async fn next(conn: &mut Conn) -> Reply {
        let call = conn.call.as_mut().expect("a lease");
        match tokio::time::timeout(Duration::from_secs(10), call.rx.message()).await {
            Ok(Ok(Some(msg))) => reply(msg),
            other => panic!("no message: {other:?}"),
        }
    }

    /// Notes the players it's to start: (session, player, epoch).
    struct Recorder {
        id: String,
        started: std::sync::Mutex<Vec<(Uuid, String, i64)>>,
        fail: bool,
    }

    impl Runner for Recorder {
        fn id(&self) -> &str {
            &self.id
        }

        fn start(&self, session: Uuid, player: &str, epoch: i64) -> anyhow::Result<()> {
            anyhow::ensure!(!self.fail, "the runner fails");
            self.started
                .lock()
                .unwrap()
                .push((session, player.into(), epoch));
            Ok(())
        }
    }

    impl Recorder {
        /// The players started for `session` so far.
        fn of(&self, session: Uuid) -> Vec<(String, i64)> {
            let started = self.started.lock().unwrap();
            started
                .iter()
                .filter(|(s, _, _)| *s == session)
                .map(|(_, p, e)| (p.clone(), *e))
                .collect()
        }
    }

    fn recorder(fail: bool) -> &'static Recorder {
        Box::leak(Box::new(Recorder {
            id: format!("test:{}", Uuid::new_v4()),
            started: Default::default(),
            fail,
        }))
    }

    /// A control on ports of its own, with `runner` (given the gRPC address players use).
    async fn start_with(runner: impl FnOnce(&str) -> &'static dyn Runner) -> Option<Ctl> {
        std::env::var_os("DIRIGENT_E2E")?;
        let url = std::env::var("DIRIGENT_CONTROL_POSTGRES")
            .expect("DIRIGENT_CONTROL_POSTGRES (the `e2e` script sets it)");
        let store = Store::connect(&url, Duration::from_secs(1)).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let grpc = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let grpc_addr = format!("127.0.0.1:{}", grpc.local_addr().unwrap().port());
        let control = control(store, Some(runner(&grpc_addr)), Duration::from_secs(1));
        tokio::spawn(sweep(control));
        tokio::spawn(async move { axum::serve(listener, router(control)).await });
        tokio::spawn(players(control).serve_with_incoming(TcpListenerStream::new(grpc)));
        Some(Ctl {
            http,
            grpc: grpc_addr,
        })
    }

    async fn start_recording(runner: &'static Recorder) -> Option<Ctl> {
        start_with(|_| runner).await
    }

    async fn put(base: &Ctl, session: Uuid, users: &[&str]) -> reqwest::StatusCode {
        put_json(base, session, serde_json::json!({ "users": users })).await
    }

    async fn put_json(base: &Ctl, session: Uuid, body: serde_json::Value) -> reqwest::StatusCode {
        reqwest::Client::new()
            .put(format!("http://{base}/sessions/{session}"))
            .json(&body)
            .send()
            .await
            .unwrap()
            .status()
    }

    async fn post(base: &Ctl, path: &str, body: Option<serde_json::Value>) -> reqwest::StatusCode {
        let req = reqwest::Client::new().post(format!("http://{base}{path}"));
        let req = match body {
            Some(b) => req.json(&b),
            None => req,
        };
        req.send().await.unwrap().status()
    }

    async fn info(base: &Ctl, session: Uuid) -> SessionInfo {
        let res = reqwest::get(format!("http://{base}/sessions/{session}"))
            .await
            .unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::OK);
        res.json().await.unwrap()
    }

    /// The session, once `done` holds for it, within 30 s.
    async fn info_until(
        base: &Ctl,
        session: Uuid,
        done: impl Fn(&SessionInfo) -> bool,
    ) -> SessionInfo {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let got = info(base, session).await;
            if done(&got) {
                return got;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out at {got:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// What a session put with the default spec acquires.
    fn acquired(epoch: i64) -> Reply {
        Reply::Acquired {
            epoch,
            spec: SessionSpec::default(),
        }
    }

    /// Asks again until the answer isn't `Held`, for at most `within`.
    async fn acquire_within(ws: &mut Conn, session: Uuid, player: &str, within: Duration) -> Reply {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let got = acquire(ws, session, player, None).await;
            if got != HELD || tokio::time::Instant::now() > deadline {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Wait up to 10 s for `runner` to have started `n` players for `session`.
    async fn started_n(runner: &Recorder, session: Uuid, n: usize) -> Vec<(String, i64)> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let got = runner.of(session);
            if got.len() >= n || tokio::time::Instant::now() > deadline {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    #[tokio::test]
    async fn e2e_putting_a_session_is_idempotent() {
        let Some(base) = start_recording(recorder(false)).await else {
            return;
        };
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
        assert_eq!(acquire(&mut ws, Uuid::new_v4(), "p", None).await, UNKNOWN);
    }

    #[tokio::test]
    async fn e2e_a_session_keeps_the_spec_it_was_made_with() {
        let Some(base) = start_recording(recorder(false)).await else {
            return;
        };
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
            Reply::Acquired {
                epoch: 1,
                spec: serde_json::from_value(spec).unwrap(),
            }
        );
    }

    #[tokio::test]
    async fn e2e_a_held_session_goes_to_the_next_player_once_given_back() {
        let Some(base) = start_recording(recorder(false)).await else {
            return;
        };
        let s = Uuid::new_v4();
        put(&base, s, &[]).await;
        let mut a = connect(&base).await;
        let mut b = connect(&base).await;
        assert_eq!(acquire(&mut a, s, "a", None).await, acquired(1));
        assert_eq!(acquire(&mut b, s, "b", None).await, HELD);
        a.close().await;
        assert_eq!(
            acquire_within(&mut b, s, "b", Duration::from_secs(5)).await,
            acquired(2)
        );
    }

    #[tokio::test]
    async fn e2e_a_dropped_player_resumes_its_lease_within_the_grace_period() {
        let Some(base) = start_recording(recorder(false)).await else {
            return;
        };
        let s = Uuid::new_v4();
        put(&base, s, &[]).await;
        let mut a = connect(&base).await;
        assert_eq!(acquire(&mut a, s, "a", None).await, acquired(1));
        drop(a); // no Close frame
        let mut b = connect(&base).await;
        assert_eq!(acquire(&mut b, s, "b", None).await, HELD);
        let mut a = connect(&base).await;
        assert_eq!(acquire(&mut a, s, "a", Some(1)).await, acquired(1));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(acquire(&mut b, s, "b", None).await, HELD);
    }

    #[tokio::test]
    async fn e2e_past_the_grace_period_the_lease_goes_to_another_player() {
        let runner = recorder(false);
        let Some(base) = start_recording(runner).await else {
            return;
        };
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
        assert_eq!(acquire(&mut a, s, "a", Some(1)).await, LOST);
        // a player control didn't start isn't started again
        assert_eq!(runner.of(s), []);
    }

    #[tokio::test]
    async fn e2e_starting_a_session_starts_a_player_for_its_reserved_lease() {
        let runner = recorder(false);
        let Some(base) = start_recording(runner).await else {
            return;
        };
        let s = Uuid::new_v4();
        let body = serde_json::json!({ "users": ["user:a"], "start": true });
        assert_eq!(put_json(&base, s, body).await, reqwest::StatusCode::CREATED);
        let [(p, 1)] = &runner.of(s)[..] else {
            panic!("{:?}", runner.of(s))
        };
        let reserved = info(&base, s).await.player.unwrap();
        assert_eq!(reserved.subject, player_subject(p));
        assert!(!reserved.attached);
        let mut ws = connect(&base).await;
        assert_eq!(acquire(&mut ws, s, p, Some(1)).await, acquired(1));
        assert!(info(&base, s).await.player.unwrap().attached);
        // active: starting it again starts nothing
        let path = format!("/sessions/{s}/start");
        assert_eq!(
            post(&base, &path, None).await,
            reqwest::StatusCode::NO_CONTENT
        );
        assert_eq!(runner.of(s).len(), 1);
    }

    #[tokio::test]
    async fn e2e_a_started_player_that_never_comes_is_started_again() {
        let runner = recorder(false);
        let Some(base) = start_recording(runner).await else {
            return;
        };
        let s = Uuid::new_v4();
        put(&base, s, &[]).await;
        let path = format!("/sessions/{s}/start");
        assert_eq!(
            post(&base, &path, None).await,
            reqwest::StatusCode::NO_CONTENT
        );
        let got = started_n(runner, s, 2).await;
        assert_eq!(got.iter().map(|(_, e)| *e).collect::<Vec<_>>(), [1, 2]);
        // the one that never came lost its lease to the next
        let mut late = connect(&base).await;
        assert_eq!(acquire(&mut late, s, &got[0].0, Some(1)).await, LOST);
        let mut next = connect(&base).await;
        assert_eq!(acquire(&mut next, s, &got[1].0, Some(2)).await, acquired(2));
    }

    #[tokio::test]
    async fn e2e_a_started_player_that_drops_is_started_again_past_the_grace_period() {
        let runner = recorder(false);
        let Some(base) = start_recording(runner).await else {
            return;
        };
        let s = Uuid::new_v4();
        put(&base, s, &[]).await;
        post(&base, &format!("/sessions/{s}/start"), None).await;
        let (p, _) = runner.of(s).remove(0);
        let mut ws = connect(&base).await;
        assert_eq!(acquire(&mut ws, s, &p, Some(1)).await, acquired(1));
        drop(ws); // crashed
        let got = started_n(runner, s, 2).await;
        assert_eq!(got.get(1).map(|(_, e)| *e), Some(2), "{got:?}");
    }

    #[tokio::test]
    async fn e2e_stopping_a_session_tells_its_player_to_detach() {
        let runner = recorder(false);
        let Some(base) = start_recording(runner).await else {
            return;
        };
        let s = Uuid::new_v4();
        put(&base, s, &[]).await;
        post(&base, &format!("/sessions/{s}/start"), None).await;
        let (p, _) = runner.of(s).remove(0);
        let mut ws = connect(&base).await;
        assert_eq!(acquire(&mut ws, s, &p, Some(1)).await, acquired(1));
        let stop = format!("/sessions/{s}/stop");
        assert_eq!(
            post(&base, &stop, None).await,
            reqwest::StatusCode::NO_CONTENT
        );
        assert_eq!(next(&mut ws).await, Reply::Detach { now: false });
        let now = Some(serde_json::json!({ "now": true }));
        assert_eq!(
            post(&base, &stop, now).await,
            reqwest::StatusCode::NO_CONTENT
        );
        assert_eq!(next(&mut ws).await, Reply::Detach { now: true });
        ws.close().await;
        info_until(&base, s, |i| i.player.is_none()).await;
        // given back, not lost: nothing is started again
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(runner.of(s).len(), 1);
    }

    #[tokio::test]
    async fn e2e_stopping_a_session_whose_player_is_away_takes_its_lease() {
        let runner = recorder(false);
        let Some(base) = start_recording(runner).await else {
            return;
        };
        let s = Uuid::new_v4();
        put(&base, s, &[]).await;
        let mut a = connect(&base).await;
        assert_eq!(acquire(&mut a, s, "a", None).await, acquired(1));
        drop(a);
        info_until(&base, s, |i| i.player.as_ref().is_some_and(|p| !p.attached)).await;
        let stop = format!("/sessions/{s}/stop");
        assert_eq!(
            post(&base, &stop, None).await,
            reqwest::StatusCode::NO_CONTENT
        );
        assert_eq!(info(&base, s).await.player, None);
        let mut a = connect(&base).await;
        assert_eq!(acquire(&mut a, s, "a", Some(1)).await, LOST);
    }

    #[tokio::test]
    async fn e2e_a_player_that_cant_be_started_leaves_the_session_idle() {
        let Some(base) = start_recording(recorder(true)).await else {
            return;
        };
        let s = Uuid::new_v4();
        put(&base, s, &[]).await;
        let path = format!("/sessions/{s}/start");
        assert_eq!(
            post(&base, &path, None).await,
            reqwest::StatusCode::INTERNAL_SERVER_ERROR
        );
        assert_eq!(info(&base, s).await.player, None);
    }

    #[tokio::test]
    async fn e2e_unknown_sessions_are_not_found() {
        let Some(base) = start_recording(recorder(false)).await else {
            return;
        };
        let s = Uuid::new_v4();
        for action in ["start", "stop"] {
            assert_eq!(
                post(&base, &format!("/sessions/{s}/{action}"), None).await,
                reqwest::StatusCode::NOT_FOUND
            );
        }
        let res = reqwest::get(format!("http://{base}/sessions/{s}"))
            .await
            .unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::NOT_FOUND);
        put(&base, s, &[]).await;
        let all: Vec<SessionInfo> = reqwest::get(format!("http://{base}/sessions"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(all.iter().any(|i| i.id == s.to_string()));
    }

    /// A control running mock players (`target/debug/player`, which `e2e` builds) in `root`, on
    /// the stack's chlive; its address.
    async fn start_local(root: &std::path::Path) -> Option<Ctl> {
        let root = root.to_path_buf();
        start_with(|addr| {
            Box::leak(Box::new(Local {
                id: format!("local:{}", root.display()),
                root: root.clone(),
                program: concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/player").into(),
                args: vec!["--mock".into()],
                control_url: format!("http://{addr}"),
            }))
        })
        .await
    }

    #[tokio::test]
    async fn e2e_local_players_start_stop_and_are_started_again_once_lost() {
        let root = tempfile::tempdir().unwrap();
        let Some(base) = start_local(root.path()).await else {
            return;
        };
        let s = Uuid::new_v4();
        let body = serde_json::json!({ "start": true });
        assert_eq!(put_json(&base, s, body).await, reqwest::StatusCode::CREATED);
        let attached = |epoch| {
            move |i: &SessionInfo| i.epoch == epoch && i.player.as_ref().is_some_and(|p| p.attached)
        };
        info_until(&base, s, attached(1)).await;
        assert!(root.path().join(s.to_string()).join("player.log").exists());

        let stop = format!("/sessions/{s}/stop");
        assert_eq!(
            post(&base, &stop, None).await,
            reqwest::StatusCode::NO_CONTENT
        );
        info_until(&base, s, |i| i.player.is_none()).await;

        let start = format!("/sessions/{s}/start");
        assert_eq!(
            post(&base, &start, None).await,
            reqwest::StatusCode::NO_CONTENT
        );
        info_until(&base, s, attached(2)).await;
        let pid = player_pid(s).expect("the player's process");
        // SAFETY: a plain kill(2)
        unsafe { libc::kill(pid, libc::SIGKILL) };
        info_until(&base, s, attached(3)).await;
        post(&base, &stop, None).await;
        info_until(&base, s, |i| i.player.is_none()).await;
    }

    #[tokio::test]
    async fn e2e_a_local_player_that_fails_gives_the_session_back() {
        let root = tempfile::tempdir().unwrap();
        let Some(base) = start_local(root.path()).await else {
            return;
        };
        let s = Uuid::new_v4();
        let missing = root.path().join("no-such-repo");
        let body = serde_json::json!({
            "start": true,
            "source": { "git": { "url": format!("file://{}", missing.display()) } },
        });
        assert_eq!(put_json(&base, s, body).await, reqwest::StatusCode::CREATED);
        info_until(&base, s, |i| i.epoch == 1 && i.player.is_none()).await;
        // given back, not lost: it isn't started again
        tokio::time::sleep(Duration::from_secs(3)).await;
        let got = info(&base, s).await;
        assert_eq!((got.epoch, got.player), (1, None));
        let log =
            std::fs::read_to_string(root.path().join(s.to_string()).join("player.log")).unwrap();
        assert!(log.contains("the workspace couldn't be made"), "{log}");
    }

    /// The pid of the player running `session`, from /proc.
    fn player_pid(session: Uuid) -> Option<libc::pid_t> {
        let needle = session.to_string();
        std::fs::read_dir("/proc").ok()?.flatten().find_map(|e| {
            let pid: libc::pid_t = e.file_name().to_str()?.parse().ok()?;
            let cmdline = std::fs::read(e.path().join("cmdline")).ok()?;
            let args: Vec<_> = cmdline.split(|b| *b == 0).collect();
            let is_player = args.first()?.ends_with(b"/player");
            let ours = args.contains(&needle.as_bytes());
            (is_player && ours).then_some(pid)
        })
    }
}
