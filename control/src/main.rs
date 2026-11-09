//! dirigent's control plane, the only writer of its Postgres. Sessions are made, started,
//! stopped and read over HTTP (`/sessions`); players hold them over gRPC (`Players.Lease`, see
//! `protocol/proto`), each call being a lease, its connection's keepalives its liveness. A session with
//! a lease is active, one without idle: starting one, control reserves the lease and starts a
//! player for it (`runner`); stopping one, it tells the player to detach. A player it started
//! that's lost without giving the lease back is started again once the lease expires.

mod auth;
mod chatgpt;
mod leases;
mod runner;
mod seal;
mod secrets;
mod token;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use auth::{Auth, OidcArgs};
use axum::extract::{FromRequestParts, Path, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use dirigent_protocol::control::pb::players_server::{Players, PlayersServer};
use dirigent_protocol::control::{
    pb, player_subject, Part, Provider, Role, SessionInfo, SessionSpec,
};
use serde::Deserialize;
use tokio::sync::{broadcast, mpsc, watch, Mutex};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status, Streaming};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::chatgpt::{ChatGpt, ChatGptArgs, LinkInfo, Model, Started as LinkStarted};
use crate::leases::{Acquire, Put, Reserve, Scope, Store};
use crate::runner::{Kube, Local, Runner};
use crate::seal::Seal;
use crate::secrets::{KeyArgs, Keys};
use crate::token::{Signer, CHLIVE, CONTROL};

/// How often a player's connection is pinged, and how long it may go unanswered before the
/// connection (and its lease's call) counts as broken.
const KEEPALIVE: Duration = Duration::from_secs(5);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a player has to ask for its session once its call is open.
const ASK_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Parser, Debug)]
#[command(
    name = "control",
    about = "dirigent's control plane",
    args_conflicts_with_subcommands = true,
    subcommand_negates_reqs = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

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
    #[arg(long, env = "DIRIGENT_CONTROL_POSTGRES", required = true)]
    postgres: Option<String>,

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

    /// Run players in KubeVirt VMs instead, each a VirtualMachineInstance made from this
    /// template (JSON; Helm's): its volume `dirigent`'s NoCloud user data is the player's
    /// environment, which the player's own lines are added to.
    #[arg(long, env = "DIRIGENT_KUBE_TEMPLATE", conflicts_with = "players")]
    kube_template: Option<PathBuf>,

    /// The namespace of the players' VMs; the service account's when absent.
    #[arg(long, env = "DIRIGENT_KUBE_NAMESPACE")]
    kube_namespace: Option<String>,

    /// Which control's VMs they are (their `dirigent.dev/runner` label): one per install.
    #[arg(long, env = "DIRIGENT_KUBE_NAME", default_value = "dirigent")]
    kube_name: String,

    /// How long a player control started has to take its lease, in seconds.
    #[arg(long, default_value_t = 60)]
    startup: u64,

    /// Lease sessions only to the players control started (who know their reserved epoch):
    /// otherwise anything that reaches the gRPC service may take an idle session.
    #[arg(long, env = "DIRIGENT_RESERVED_ONLY")]
    reserved_only: bool,

    /// How callers of the HTTP API are known (besides by control's own tokens): `declared`, by
    /// the `dirigent-user` header, unverified (dev only; `POST /token` then signs a chlive token
    /// for them); `oidc`, by an OpenID provider's ID token (`--oidc-*`).
    #[arg(long, value_enum, default_value_t = AuthMode::Declared)]
    auth: AuthMode,

    #[command(flatten)]
    oidc: OidcArgs,

    #[command(flatten)]
    chatgpt: ChatGptArgs,

    #[command(flatten)]
    keys: KeyArgs,

    /// The key control signs credentials with: Ed25519, PKCS#8 PEM (`control keygen`).
    #[arg(long, env = "DIRIGENT_SIGNING_KEY", required = true)]
    signing_key: Option<PathBuf>,

    /// The `iss` of control's credentials (chlive's issuer for them says the same).
    #[arg(long, env = "DIRIGENT_ISSUER", default_value = "dirigent-control")]
    issuer: String,

    /// How long control's credentials are good for, in seconds; a lease's are sent afresh
    /// halfway.
    #[arg(long, default_value_t = 600)]
    token_ttl: u64,

    /// More arguments for each player (e.g. `-- --mock`).
    #[arg(last = true)]
    player_args: Vec<String>,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Make a signing key in `dir` (`key.pem`), unless it has one, and write its public half
    /// (`jwks.json`, chlive's `jwks_file`).
    Keygen { dir: PathBuf },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum AuthMode {
    Declared,
    Oidc,
}

struct Control {
    store: Store,
    auth: Auth,
    /// The ChatGPT plans linked, which sessions may run on; none without a link key.
    chatgpt: Option<ChatGpt>,
    /// Users' API keys and secrets.
    keys: Keys,
    /// What changed of leases' credentials, which their holders send again at once.
    changed: broadcast::Sender<Changed>,
    runner: Option<&'static dyn Runner>,
    startup: Duration,
    /// Whether only the players it started get leases (`--reserved-only`).
    reserved_only: bool,
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
    if let Some(Command::Keygen { dir }) = &cli.command {
        return token::keygen(dir);
    }
    let postgres = cli.postgres.as_deref().expect("required");
    let signing_key = cli.signing_key.as_deref().expect("required");
    let signer = Signer::load(signing_key, &cli.issuer, Duration::from_secs(cli.token_ttl))?;
    let chatgpt = ChatGpt::new(&cli.chatgpt, signer.kid())?;
    let seal = cli
        .chatgpt
        .link_key
        .as_deref()
        .map(Seal::load)
        .transpose()?;
    let keys = Keys::new(&cli.keys, seal)?;
    let oidc = match cli.auth {
        AuthMode::Declared => None,
        AuthMode::Oidc => Some(auth::Oidc::start(&cli.oidc).await?),
    };
    let store = Store::connect(postgres, Duration::from_secs(cli.grace)).await?;
    let detached = store.detach_all().await?;
    let kube: Option<&'static Kube> = match &cli.kube_template {
        Some(template) => Some(Box::leak(Box::new(
            Kube::new(&cli.kube_name, cli.kube_namespace.as_deref(), template).await?,
        ))),
        None => None,
    };
    let runner: Option<&'static dyn Runner> = match &cli.players {
        _ if kube.is_some() => kube.map(|k| k as &'static dyn Runner),
        Some(root) => {
            std::fs::create_dir_all(root)?;
            let root = root.canonicalize()?;
            Some(Box::leak(Box::new(Local {
                id: format!("local:{}", root.display()),
                root,
                program: cli.player.clone(),
                args: cli.player_args.clone(),
                control_url: format!("http://{}", cli.grpc_listen),
                control_http: format!("http://{}", cli.listen),
            })))
        }
        None => None,
    };
    let control = control(
        store,
        Auth { signer, oidc },
        chatgpt,
        keys,
        runner,
        Duration::from_secs(cli.startup),
        cli.reserved_only,
    );
    tokio::spawn(sweep(control));
    if let Some(kube) = kube {
        tokio::spawn(kube.clear(&control.store, control.startup, Duration::from_secs(30)));
    }
    let listener = tokio::net::TcpListener::bind(cli.listen).await?;
    let grpc = tokio::net::TcpListener::bind(cli.grpc_listen).await?;
    info!(
        listen = %cli.listen,
        grpc = %cli.grpc_listen,
        grace = cli.grace,
        runner = runner.map(|r| r.id()),
        auth = ?cli.auth,
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
    auth: Auth,
    chatgpt: Option<ChatGpt>,
    keys: Keys,
    runner: Option<&'static dyn Runner>,
    startup: Duration,
    reserved_only: bool,
) -> &'static Control {
    Box::leak(Box::new(Control {
        store,
        auth,
        chatgpt,
        keys,
        changed: broadcast::channel(64).0,
        runner,
        startup,
        reserved_only,
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
        .route(
            "/sessions/{id}/viewers/{subject}",
            axum::routing::put(add_viewer).delete(remove_viewer),
        )
        .route("/auth", get(auth_mode))
        .route("/me", get(me))
        .route(
            "/sessions/{id}/secrets/{name}",
            axum::routing::put(put_session_secret).delete(delete_session_secret),
        )
        .route("/sessions/{id}/secrets", get(list_session_secrets))
        .route("/secrets", get(list_user_secrets))
        .route(
            "/secrets/{name}",
            axum::routing::put(put_user_secret).delete(delete_user_secret),
        )
        .route(
            "/links/anthropic",
            axum::routing::put(put_anthropic_key).delete(delete_anthropic_key),
        )
        .route("/links/chatgpt", axum::routing::delete(unlink_chatgpt))
        .route("/links/chatgpt/start", post(start_link))
        .route("/links/chatgpt/finish", post(finish_link))
        .route("/links/chatgpt/models", get(chatgpt_models))
        .route("/token", post(chlive_token))
        .route("/.well-known/jwks.json", get(jwks))
        .with_state(control)
}

/// How the API's callers are known, for clients to sign in by: `{"mode": "declared"}`, or
/// `{"mode": "oidc", "google_client_id": <the web's client id>, "cli": {"client_id",
/// "client_secret"}}`, the CLI's (an installed app's, whose secret isn't one), when it has one.
async fn auth_mode(State(control): State<&'static Control>) -> Json<serde_json::Value> {
    Json(match &control.auth.oidc {
        None => serde_json::json!({ "mode": "declared" }),
        Some(oidc) => serde_json::json!({
            "mode": "oidc",
            "google_client_id": oidc.client_id(),
            "cli": oidc.cli().map(|(id, secret)| {
                serde_json::json!({ "client_id": id, "client_secret": secret })
            }),
        }),
    })
}

/// The caller, the ChatGPT plan they linked and the Anthropic key they gave: `{"subject",
/// "chatgpt": {"email", "broken"}, "anthropic": {"hint"}}`.
async fn me(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
) -> Result<Json<serde_json::Value>, Failed> {
    let chatgpt = match &control.chatgpt {
        Some(c) => c
            .info(&control.store, &caller)
            .await
            .map_err(|e| failed("reading the ChatGPT link", e))?,
        None => None,
    };
    let anthropic = match &control.keys.seal {
        Some(_) => control
            .store
            .api_key(&caller, secrets::ANTHROPIC)
            .await
            .map_err(|e| failed("reading the Anthropic key", e))?
            .map(|(_, hint)| serde_json::json!({ "hint": hint })),
        None => None,
    };
    Ok(Json(serde_json::json!({
        "subject": caller,
        "chatgpt": chatgpt,
        "anthropic": anthropic,
    })))
}

fn linking(control: &Control) -> Result<&ChatGpt, Failed> {
    control.chatgpt.as_ref().ok_or((
        StatusCode::NOT_IMPLEMENTED,
        "this control links no ChatGPT plans (no --link-key)".into(),
    ))
}

#[derive(Deserialize)]
struct StartLink {
    /// The loopback port the CLI waits on for the sign-in to come back.
    port: u16,
}

/// A ChatGPT sign-in for the caller to make in their browser.
async fn start_link(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
    Json(body): Json<StartLink>,
) -> Result<Json<LinkStarted>, Failed> {
    let chatgpt = linking(control)?;
    match chatgpt.start(&control.store, &caller, body.port).await {
        Ok(started) => Ok(Json(started)),
        Err(e) => Err(failed("starting a ChatGPT sign-in", e)),
    }
}

/// What the sign-in came back to the loopback with (its query: more is ignored).
#[derive(Deserialize)]
struct FinishLink {
    code: String,
    state: String,
    client_id: Option<String>,
}

async fn finish_link(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
    Json(body): Json<FinishLink>,
) -> Result<Json<LinkInfo>, Failed> {
    let chatgpt = linking(control)?;
    let done = chatgpt
        .finish(
            &control.store,
            &caller,
            &body.code,
            &body.state,
            body.client_id.as_deref(),
        )
        .await;
    done.map(Json).map_err(|why| (StatusCode::BAD_REQUEST, why))
}

async fn unlink_chatgpt(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
) -> Result<StatusCode, Failed> {
    let chatgpt = linking(control)?;
    match chatgpt.unlink(&control.store, &caller).await {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err(not_found()),
        Err(e) => Err(failed("unlinking ChatGPT", e)),
    }
}

/// The models the caller's plan has.
async fn chatgpt_models(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
) -> Result<Json<Vec<Model>>, Failed> {
    let chatgpt = linking(control)?;
    chatgpt
        .models(&control.store, &caller)
        .await
        .map(Json)
        .map_err(|why| (StatusCode::BAD_GATEWAY, why))
}

/// In `declared` auth (dev), a chlive credential for the caller, standing in for the OpenID
/// provider's: `{"token": …}`.
async fn chlive_token(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
) -> Result<Json<serde_json::Value>, Failed> {
    if control.auth.oidc.is_some() {
        return Err((
            StatusCode::NOT_FOUND,
            "chlive takes the OpenID provider's tokens".into(),
        ));
    }
    let token = control.auth.signer.sign(&caller, &[CHLIVE], None);
    Ok(Json(serde_json::json!({ "token": token })))
}

/// Control's public key, which its credentials are verified with.
async fn jwks(State(control): State<&'static Control>) -> Json<serde_json::Value> {
    Json(control.auth.signer.jwks())
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
    if let Err(e) = runner.start(session, &player, epoch).await {
        control.store.drop_lease(session).await?;
        return Err(e);
    }
    Ok(Started::Player(epoch))
}

/// Who calls the HTTP API (a subject, `user:<name>`), as [`Auth::caller`] tells.
struct Caller(String);

impl FromRequestParts<&'static Control> for Caller {
    type Rejection = Failed;

    async fn from_request_parts(
        parts: &mut Parts,
        control: &&'static Control,
    ) -> Result<Self, Failed> {
        match control.auth.caller(&parts.headers).await {
            Ok(caller) => Ok(Caller(caller)),
            Err(why) => Err((StatusCode::UNAUTHORIZED, why)),
        }
    }
}

#[derive(Deserialize)]
struct NewSession {
    /// Who else may read it.
    #[serde(default)]
    viewers: Vec<String>,
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

fn not_owner() -> Failed {
    (StatusCode::FORBIDDEN, "only the session's owner may".into())
}

/// Unless `caller` owns the session `id`: why not (it views it, or it's none of theirs).
async fn owned(control: &Control, id: Uuid, caller: &str) -> Result<(), Failed> {
    match control.store.role(id, caller).await {
        Ok(Some(Role::Owner)) => Ok(()),
        Ok(Some(Role::Viewer)) => Err(not_owner()),
        Ok(None) => Err(not_found()),
        Err(e) => Err(failed(&format!("reading the session {id}"), e)),
    }
}

/// Make the session `id`, the caller's, or find it as they made it.
async fn put_session(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
    Path(id): Path<Uuid>,
    Json(body): Json<NewSession>,
) -> Result<StatusCode, Failed> {
    body.spec
        .check()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    if body.spec.provider == Provider::Chatgpt {
        let link = linking(control)?
            .info(&control.store, &caller)
            .await
            .map_err(|e| failed("reading the ChatGPT link", e))?;
        let why = match link {
            None => Some("link a ChatGPT plan first (dirigent link chatgpt)".to_string()),
            Some(LinkInfo {
                broken: Some(why), ..
            }) => Some(format!(
                "the ChatGPT link is broken: {why} (link it again: dirigent link chatgpt)"
            )),
            Some(_) => None,
        };
        if let Some(why) = why {
            return Err((StatusCode::UNPROCESSABLE_ENTITY, why));
        }
    }
    let put = control
        .store
        .put_session(id, &body.spec, &caller, &body.viewers)
        .await;
    let status = match put {
        Ok(Put::Made) => {
            info!(session = %id, owner = caller, viewers = ?body.viewers, spec = ?body.spec, "session made");
            StatusCode::CREATED
        }
        Ok(Put::Exists) => StatusCode::OK,
        Ok(Put::Conflict) => {
            return Err((
                StatusCode::CONFLICT,
                "the session exists, with another spec".into(),
            ))
        }
        Ok(Put::NotOwner) => {
            return Err((StatusCode::FORBIDDEN, "the session is another's".into()))
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

/// The sessions the caller owns or views.
async fn list_sessions(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
) -> Result<Json<Vec<SessionInfo>>, Failed> {
    match control.store.sessions(&caller).await {
        Ok(sessions) => Ok(Json(sessions)),
        Err(e) => Err(failed("listing the sessions", e)),
    }
}

async fn get_session(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<SessionInfo>, Failed> {
    match control.store.session(id, &caller).await {
        Ok(Some(info)) => Ok(Json(info)),
        Ok(None) => Err(not_found()),
        Err(e) => Err(failed(&format!("reading the session {id}"), e)),
    }
}

/// Start (or resume) the session: a player for it, unless it's active.
async fn start_session(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Failed> {
    owned(control, id, &caller).await?;
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
    Caller(caller): Caller,
    Path(id): Path<Uuid>,
    body: Option<Json<Stop>>,
) -> Result<StatusCode, Failed> {
    owned(control, id, &caller).await?;
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
        Ok(false) => Ok(StatusCode::NO_CONTENT),
        Err(e) => Err(failed(&format!("stopping the session {id}"), e)),
    }
}

/// Let `subject` read the session (the caller's).
async fn add_viewer(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
    Path((id, subject)): Path<(Uuid, String)>,
) -> Result<StatusCode, Failed> {
    owned(control, id, &caller).await?;
    match control.store.add_viewer(id, &subject).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(e) => Err(failed(&format!("adding a viewer to {id}"), e)),
    }
}

async fn remove_viewer(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
    Path((id, subject)): Path<(Uuid, String)>,
) -> Result<StatusCode, Failed> {
    owned(control, id, &caller).await?;
    match control.store.remove_viewer(id, &subject).await {
        Ok(()) => Ok(StatusCode::NO_CONTENT),
        Err(e) => Err(failed(&format!("removing a viewer of {id}"), e)),
    }
}

fn sealing(control: &Control) -> Result<&Seal, Failed> {
    control.keys.seal.as_ref().ok_or((
        StatusCode::NOT_IMPLEMENTED,
        "this control keeps no keys or secrets (no --link-key)".into(),
    ))
}

/// What a sealed thing changes, for the leases it reaches.
fn changed(control: &Control, what: Changed) {
    // no lease held, no one listens
    let _ = control.changed.send(what);
}

#[derive(Deserialize)]
struct Key {
    key: String,
}

/// The caller's Anthropic API key is this one, once Anthropic takes it: `{"hint"}`.
async fn put_anthropic_key(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
    Json(Key { key }): Json<Key>,
) -> Result<Json<serde_json::Value>, Failed> {
    let seal = sealing(control)?;
    let key = key.trim();
    if key.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "the key is empty".into()));
    }
    control
        .keys
        .check_anthropic(key)
        .await
        .map_err(|why| (StatusCode::BAD_REQUEST, why))?;
    let hint = secrets::hint(key);
    control
        .store
        .put_api_key(
            &caller,
            secrets::ANTHROPIC,
            &seal.seal(key.as_bytes()),
            &hint,
        )
        .await
        .map_err(|e| failed("keeping an Anthropic key", e))?;
    changed(control, Changed::Owner(caller));
    Ok(Json(serde_json::json!({ "hint": hint })))
}

async fn delete_anthropic_key(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
) -> Result<StatusCode, Failed> {
    sealing(control)?;
    match control
        .store
        .delete_api_key(&caller, secrets::ANTHROPIC)
        .await
    {
        Ok(true) => {
            changed(control, Changed::Owner(caller));
            Ok(StatusCode::NO_CONTENT)
        }
        Ok(false) => Err((StatusCode::NOT_FOUND, "no Anthropic key was given".into())),
        Err(e) => Err(failed("forgetting an Anthropic key", e)),
    }
}

#[derive(Deserialize)]
struct Secret {
    value: String,
}

async fn list_secrets(control: &Control, scope: Scope<'_>) -> Result<Json<Vec<String>>, Failed> {
    sealing(control)?;
    match control.store.secret_names(scope).await {
        Ok(names) => Ok(Json(names)),
        Err(e) => Err(failed("listing secrets", e)),
    }
}

async fn put_secret(
    control: &Control,
    scope: Scope<'_>,
    name: &str,
    value: &str,
) -> Result<StatusCode, Failed> {
    let seal = sealing(control)?;
    secrets::check_name(name).map_err(|why| (StatusCode::BAD_REQUEST, why))?;
    secrets::check_value(value).map_err(|why| (StatusCode::BAD_REQUEST, why))?;
    control
        .store
        .put_secret(scope, name, &seal.seal(value.as_bytes()))
        .await
        .map_err(|e| failed("keeping a secret", e))?;
    changed(control, scope.into());
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_secret(
    control: &Control,
    scope: Scope<'_>,
    name: &str,
) -> Result<StatusCode, Failed> {
    sealing(control)?;
    match control.store.delete_secret(scope, name).await {
        Ok(true) => {
            changed(control, scope.into());
            Ok(StatusCode::NO_CONTENT)
        }
        Ok(false) => Err((StatusCode::NOT_FOUND, format!("no secret {name}"))),
        Err(e) => Err(failed("forgetting a secret", e)),
    }
}

impl From<Scope<'_>> for Changed {
    fn from(scope: Scope<'_>) -> Changed {
        match scope {
            Scope::User(subject) => Changed::Owner(subject.to_string()),
            Scope::Session(session) => Changed::Session(session),
        }
    }
}

/// The names of the caller's secrets for all their sessions.
async fn list_user_secrets(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
) -> Result<Json<Vec<String>>, Failed> {
    list_secrets(control, Scope::User(&caller)).await
}

async fn put_user_secret(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
    Path(name): Path<String>,
    Json(Secret { value }): Json<Secret>,
) -> Result<StatusCode, Failed> {
    put_secret(control, Scope::User(&caller), &name, &value).await
}

async fn delete_user_secret(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
    Path(name): Path<String>,
) -> Result<StatusCode, Failed> {
    delete_secret(control, Scope::User(&caller), &name).await
}

/// The names of the session's own secrets (the caller's).
async fn list_session_secrets(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<String>>, Failed> {
    owned(control, id, &caller).await?;
    list_secrets(control, Scope::Session(id)).await
}

async fn put_session_secret(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
    Path((id, name)): Path<(Uuid, String)>,
    Json(Secret { value }): Json<Secret>,
) -> Result<StatusCode, Failed> {
    owned(control, id, &caller).await?;
    put_secret(control, Scope::Session(id), &name, &value).await
}

async fn delete_session_secret(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
    Path((id, name)): Path<(Uuid, String)>,
) -> Result<StatusCode, Failed> {
    owned(control, id, &caller).await?;
    delete_secret(control, Scope::Session(id), &name).await
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
        if control.reserved_only && ask.epoch.is_none() {
            return Err(Status::permission_denied(
                "control leases sessions only to the players it started",
            ));
        }
        let subject = player_subject(&ask.player);
        let conn = control.connections.fetch_add(1, Ordering::Relaxed);
        let mut attached = control.attached.lock().await;
        let (epoch, spec, owner) = match control.store.acquire(session, &subject, ask.epoch).await {
            Ok(Acquire::Acquired(epoch, spec, owner)) => (epoch, spec, owner),
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
        let held = Held {
            session,
            subject,
            owner,
            // a dirigent acts as its owner
            delegate: spec.part == Part::Dirigent,
            provider: spec.provider,
            conn,
        };
        let acquired = pb::ToPlayer {
            msg: Some(pb::to_player::Msg::Acquired(pb::Acquired {
                epoch,
                spec: Some(spec.into()),
                tokens: Some(tokens(control, &held, false).await),
            })),
        };
        tx.send(Ok(acquired)).await.expect("the receiver is here");
        tokio::spawn(hold(control, held, inbound, tx, detached));
        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

/// The player's credentials: as its subject, and, a dirigent's, as its owner too; a
/// ChatGPT-plan session's, an access token of its owner's plan (`rejected`: not the one handed
/// out last, which OpenAI refused); an Anthropic session's, its owner's API key or the
/// install's; and the session's secrets.
async fn tokens(control: &Control, held: &Held, rejected: bool) -> pb::Tokens {
    let signer = &control.auth.signer;
    let subject = held.subject.as_str();
    let owner = held.owner.as_str();
    let (openai, openai_unavailable) = match (held.provider, &control.chatgpt) {
        (Provider::Anthropic, _) => (None, None),
        (Provider::Chatgpt, None) => (None, Some("this control links no ChatGPT plans".into())),
        (Provider::Chatgpt, Some(chatgpt)) => {
            match chatgpt.access(&control.store, owner, rejected).await {
                Ok(token) => (Some(token), None),
                Err(why) => {
                    warn!(session = %held.session, owner, "no ChatGPT access token: {why}");
                    (None, Some(why.to_string()))
                }
            }
        }
    };
    let (anthropic, anthropic_unavailable) = match held.provider {
        Provider::Chatgpt => (None, None),
        Provider::Anthropic => match anthropic_key(control, owner).await {
            Ok(key) => (Some(key), None),
            Err(why) => (None, Some(why)),
        },
    };
    let secrets = match session_secrets(control, held.session).await {
        Ok(secrets) => secrets,
        Err(e) => {
            error!(session = %held.session, "reading the session's secrets: {e:#}");
            Default::default()
        }
    };
    pb::Tokens {
        player: signer.sign(subject, &[CHLIVE], None),
        owner: held
            .delegate
            .then(|| signer.sign(owner, &[CHLIVE, CONTROL], Some(subject))),
        openai,
        openai_unavailable,
        anthropic,
        anthropic_unavailable,
        secrets,
    }
}

/// The Anthropic key `owner`'s sessions run on, their own; or why there's none.
async fn anthropic_key(control: &Control, owner: &str) -> Result<String, String> {
    let keys = &control.keys;
    let own = match &keys.seal {
        Some(seal) => match control.store.api_key(owner, secrets::ANTHROPIC).await {
            Ok(Some((sealed, _))) => Some(seal.open(&sealed)),
            Ok(None) => None,
            Err(e) => Some(Err(e)),
        },
        None => None,
    };
    match own {
        Some(Ok(key)) => Ok(key),
        Some(Err(e)) => {
            error!(owner, "reading an Anthropic key: {e:#}");
            Err(format!("the Anthropic API key couldn't be read: {e:#}"))
        }
        None => Err(
            "no Anthropic API key: add yours in the web's account menu, or with \
                             `dirigent link anthropic`"
                .into(),
        ),
    }
}

/// A session's secrets, opened, by name.
async fn session_secrets(
    control: &Control,
    session: Uuid,
) -> anyhow::Result<HashMap<String, String>> {
    let Some(seal) = &control.keys.seal else {
        return Ok(HashMap::new());
    };
    control
        .store
        .session_secrets(session)
        .await?
        .into_iter()
        .map(|(name, sealed)| Ok((name, seal.open(&sealed)?)))
        .collect()
}

/// A lease held on a call.
struct Held {
    session: Uuid,
    /// The player's.
    subject: String,
    /// The session's owner, whose ChatGPT plan or API key pays for its model calls.
    owner: String,
    /// Whether it acts as its owner: a dirigent does.
    delegate: bool,
    provider: Provider,
    /// Which connection.
    conn: u64,
}

/// What changed of what a lease's credentials are made of: a user's (their key, or their
/// secrets for all their sessions), or a session's (its secrets).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Changed {
    Owner(String),
    Session(Uuid),
}

impl Held {
    fn affected(&self, changed: &Changed) -> bool {
        match changed {
            Changed::Owner(owner) => *owner == self.owner,
            Changed::Session(session) => *session == self.session,
        }
    }
}

/// A held lease, until its call ends: then given back (the player ended its stream) or kept
/// for the grace period (the call broke). Meanwhile the player's credentials are sent afresh
/// halfway through their life, and at once when what they're made of changes (a key, a
/// secret). Given back after a `Handoff`, the next player is started at once, unless the session
/// was stopped.
async fn hold(
    control: &'static Control,
    held: Held,
    mut inbound: Streaming<pb::FromPlayer>,
    tx: mpsc::Sender<Result<pb::ToPlayer, Status>>,
    mut detached: watch::Receiver<Option<bool>>,
) {
    let (session, conn) = (held.session, held.conn);
    let subject = held.subject.as_str();
    let half = control.auth.signer.ttl / 2;
    let mut renew = tokio::time::interval_at(tokio::time::Instant::now() + half, half);
    let mut changes = control.changed.subscribe();
    let mut handoff = false;
    let closed = loop {
        tokio::select! {
            changed = changes.recv() => {
                // missed some: any may have been this lease's
                let ours = match changed {
                    Ok(changed) => held.affected(&changed),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => false,
                };
                if !ours {
                    continue;
                }
                let tokens = pb::ToPlayer {
                    msg: Some(pb::to_player::Msg::Tokens(tokens(control, &held, false).await)),
                };
                if tx.send(Ok(tokens)).await.is_err() {
                    break false;
                }
            }
            _ = renew.tick() => {
                let tokens = pb::ToPlayer {
                    msg: Some(pb::to_player::Msg::Tokens(tokens(control, &held, false).await)),
                };
                if tx.send(Ok(tokens)).await.is_err() {
                    break false;
                }
            }
            msg = inbound.message() => match msg {
                Ok(Some(pb::FromPlayer { msg: Some(pb::from_player::Msg::Handoff(_)) })) => {
                    handoff = true;
                }
                Ok(Some(pb::FromPlayer { msg: Some(pb::from_player::Msg::RefreshTokens(r)) })) => {
                    let tokens = pb::ToPlayer {
                        msg: Some(pb::to_player::Msg::Tokens(tokens(control, &held, r.rejected).await)),
                    };
                    if tx.send(Ok(tokens)).await.is_err() {
                        break false;
                    }
                }
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
        true => control.store.release(session, subject).await,
        false => control.store.detach(session, subject).await,
    };
    match r {
        Ok(()) if closed => info!(session = %session, subject, handoff, "lease given back"),
        Ok(()) => info!(session = %session, subject, "player dropped; lease kept for now"),
        Err(e) => {
            error!(session = %session, subject, "ending a lease: {e:#}");
            return;
        }
    }
    if closed && handoff && detached.borrow().is_none() {
        match start(control, session).await {
            Ok(started) => info!(session = %session, ?started, "handed over"),
            Err(e) => error!(session = %session, "starting the next player: {e:#}"),
        }
    }
}

/// Against the running stack's Postgres (`dnvr up`, then `e2e`), with a control of its own, a
/// grace period and startup time of a second, and a runner that only notes the players it's to
/// start (or the real one, with mock players); skipped unless `DIRIGENT_E2E` is set. Each test
/// has sessions of its own. (Starting control detaches every lease, the stack's too, so that
/// isn't tested here.)
#[cfg(test)]
mod e2e {
    use std::sync::Arc;

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
        /// Fresh credentials, as their subjects: the player's, and, a dirigent's, its owner's.
        Tokens {
            player: String,
            owner: Option<String>,
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
        /// What the last `Acquired` came with.
        tokens: Option<pb::Tokens>,
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

        /// Hand the session over (its machine is going), then end the stream.
        async fn handoff(self) {
            let call = self.call.as_ref().expect("a lease");
            let handoff = pb::FromPlayer {
                msg: Some(pb::from_player::Msg::Handoff(pb::Handoff {})),
            };
            call.tx.send(handoff).await.unwrap();
            self.close().await;
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
            pb::to_player::Msg::Tokens(t) => Reply::Tokens {
                player: claims(&t.player).sub,
                owner: t.owner.as_deref().map(|o| claims(o).sub),
            },
        }
    }

    /// A token's claims, unverified.
    fn claims(token: &str) -> token::Claims {
        jsonwebtoken::dangerous::insecure_decode_claims(token).unwrap()
    }

    async fn connect(ctl: &Ctl) -> Conn {
        Conn {
            grpc: ctl.grpc.clone(),
            call: None,
            tokens: None,
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
            Ok(Ok(Some(msg))) => {
                if let Some(pb::to_player::Msg::Acquired(a)) = &msg.msg {
                    conn.tokens = a.tokens.clone();
                }
                reply(msg)
            }
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

    #[async_trait::async_trait]
    impl Runner for Recorder {
        fn id(&self) -> &str {
            &self.id
        }

        async fn start(&self, session: Uuid, player: &str, epoch: i64) -> anyhow::Result<()> {
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
    async fn start_with(runner: impl FnOnce(&Ctl) -> &'static dyn Runner) -> Option<Ctl> {
        start_full(None, Duration::from_secs(600), false, None, None, runner).await
    }

    /// The same, in `oidc` auth with `oidc`, its credentials good for `ttl`. It signs with the
    /// stack's key (`DIRIGENT_SIGNING_KEY`), which chlive takes, as the stack's control.
    async fn start_full(
        oidc: Option<Arc<auth::Oidc>>,
        ttl: Duration,
        reserved_only: bool,
        chatgpt: Option<ChatGptArgs>,
        keys: Option<(KeyArgs, Seal)>,
        runner: impl FnOnce(&Ctl) -> &'static dyn Runner,
    ) -> Option<Ctl> {
        std::env::var_os("DIRIGENT_E2E")?;
        let key = std::env::var("DIRIGENT_SIGNING_KEY")
            .expect("DIRIGENT_SIGNING_KEY (the `e2e` script sets it)");
        let signer = Signer::load(key.as_ref(), "dirigent-control", ttl).unwrap();
        let keys = match keys {
            Some((args, seal)) => Keys::new(&args, Some(seal)).unwrap(),
            None => {
                let seal = chatgpt.as_ref().and_then(|a| a.link_key.as_deref());
                let args = KeyArgs {
                    anthropic_api: "http://127.0.0.1:1".into(),
                };
                Keys::new(&args, seal.map(|k| Seal::load(k).unwrap())).unwrap()
            }
        };
        let chatgpt = chatgpt.map(|args| ChatGpt::new(&args, signer.kid()).unwrap().unwrap());
        let url = std::env::var("DIRIGENT_CONTROL_POSTGRES")
            .expect("DIRIGENT_CONTROL_POSTGRES (the `e2e` script sets it)");
        let store = Store::connect(&url, Duration::from_secs(1)).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let grpc = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let grpc_addr = format!("127.0.0.1:{}", grpc.local_addr().unwrap().port());
        let ctl = Ctl {
            http,
            grpc: grpc_addr,
        };
        let control = control(
            store,
            Auth { signer, oidc },
            chatgpt,
            keys,
            Some(runner(&ctl)),
            Duration::from_secs(1),
            reserved_only,
        );
        tokio::spawn(sweep(control));
        tokio::spawn(async move { axum::serve(listener, router(control)).await });
        tokio::spawn(players(control).serve_with_incoming(TcpListenerStream::new(grpc)));
        Some(ctl)
    }

    async fn start_recording(runner: &'static Recorder) -> Option<Ctl> {
        start_with(|_| runner).await
    }

    /// Who the tests call the API as, unless they say.
    const A: &str = "user:a";
    const B: &str = "user:b";

    /// A request to the API as `user`.
    fn call(
        user: &str,
        method: reqwest::Method,
        base: &Ctl,
        path: &str,
    ) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .request(method, format!("http://{base}{path}"))
            .header(auth::CALLER, user)
    }

    /// Make `session`, `A`'s, with the default spec.
    async fn put(base: &Ctl, session: Uuid) -> reqwest::StatusCode {
        put_json(base, session, A, serde_json::json!({})).await
    }

    async fn put_json(
        base: &Ctl,
        session: Uuid,
        user: &str,
        body: serde_json::Value,
    ) -> reqwest::StatusCode {
        let path = format!("/sessions/{session}");
        let req = call(user, reqwest::Method::PUT, base, &path).json(&body);
        req.send().await.unwrap().status()
    }

    async fn post(base: &Ctl, path: &str, body: Option<serde_json::Value>) -> reqwest::StatusCode {
        post_as(A, base, path, body).await
    }

    async fn post_as(
        user: &str,
        base: &Ctl,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> reqwest::StatusCode {
        let req = call(user, reqwest::Method::POST, base, path);
        let req = match body {
            Some(b) => req.json(&b),
            None => req,
        };
        req.send().await.unwrap().status()
    }

    async fn info(base: &Ctl, session: Uuid) -> SessionInfo {
        let res = call(
            A,
            reqwest::Method::GET,
            base,
            &format!("/sessions/{session}"),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::OK);
        res.json().await.unwrap()
    }

    /// The sessions `user` sees.
    async fn list(base: &Ctl, user: &str) -> Vec<SessionInfo> {
        let res = call(user, reqwest::Method::GET, base, "/sessions")
            .send()
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
        assert_eq!(put(&base, s).await, reqwest::StatusCode::CREATED);
        let viewers = serde_json::json!({ "viewers": [B] });
        assert_eq!(
            put_json(&base, s, A, viewers.clone()).await,
            reqwest::StatusCode::OK
        );
        let got = info(&base, s).await;
        assert_eq!((got.owner.as_str(), got.viewers), (A, vec![B.to_string()]));
        // another's
        assert_eq!(
            put_json(&base, s, B, viewers).await,
            reqwest::StatusCode::FORBIDDEN
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
        assert_eq!(
            put_json(&base, s, A, spec.clone()).await,
            reqwest::StatusCode::CREATED
        );
        assert_eq!(
            put_json(&base, s, A, spec.clone()).await,
            reqwest::StatusCode::OK
        );
        assert_eq!(put(&base, s).await, reqwest::StatusCode::CONFLICT);
        // a dirigent has no source
        let dirigent = serde_json::json!({ "part": "dirigent", "source": spec["source"] });
        assert_eq!(
            put_json(&base, Uuid::new_v4(), A, dirigent).await,
            reqwest::StatusCode::UNPROCESSABLE_ENTITY
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
        put(&base, s).await;
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
        put(&base, s).await;
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
        put(&base, s).await;
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
        let body = serde_json::json!({ "start": true });
        assert_eq!(
            put_json(&base, s, A, body).await,
            reqwest::StatusCode::CREATED
        );
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
        put(&base, s).await;
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
        put(&base, s).await;
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
        put(&base, s).await;
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
    async fn e2e_a_session_handed_over_goes_to_the_next_player_at_once_unless_stopped() {
        let runner = recorder(false);
        let Some(base) = start_recording(runner).await else {
            return;
        };
        let s = Uuid::new_v4();
        put(&base, s).await;
        post(&base, &format!("/sessions/{s}/start"), None).await;
        let (p, _) = runner.of(s).remove(0);
        let mut ws = connect(&base).await;
        assert_eq!(acquire(&mut ws, s, &p, Some(1)).await, acquired(1));
        ws.handoff().await;
        let got = started_n(runner, s, 2).await;
        assert_eq!(got[1].1, 2, "{got:?}");

        // stopped first, it stays stopped
        let mut ws = connect(&base).await;
        assert_eq!(acquire(&mut ws, s, &got[1].0, Some(2)).await, acquired(2));
        post(&base, &format!("/sessions/{s}/stop"), None).await;
        assert_eq!(next(&mut ws).await, Reply::Detach { now: false });
        ws.handoff().await;
        info_until(&base, s, |i| i.player.is_none()).await;
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(runner.of(s).len(), 2);
    }

    #[tokio::test]
    async fn e2e_reserved_only_control_leases_only_the_players_it_started() {
        let runner = recorder(false);
        let Some(base) =
            start_full(None, Duration::from_secs(600), true, None, None, |_| runner).await
        else {
            return;
        };
        let s = Uuid::new_v4();
        put(&base, s).await;
        let mut ws = connect(&base).await;
        let anyone = Uuid::new_v4().to_string();
        assert_eq!(
            acquire(&mut ws, s, &anyone, None).await,
            Reply::Refused(Code::PermissionDenied)
        );
        post(&base, &format!("/sessions/{s}/start"), None).await;
        let (p, _) = runner.of(s).remove(0);
        assert_eq!(acquire(&mut ws, s, &p, Some(1)).await, acquired(1));
        ws.close().await;
    }

    #[tokio::test]
    async fn e2e_stopping_a_session_whose_player_is_away_takes_its_lease() {
        let runner = recorder(false);
        let Some(base) = start_recording(runner).await else {
            return;
        };
        let s = Uuid::new_v4();
        put(&base, s).await;
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
        put(&base, s).await;
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
        let res = call(A, reqwest::Method::GET, &base, &format!("/sessions/{s}"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::NOT_FOUND);
        put(&base, s).await;
        assert!(list(&base, A).await.iter().any(|i| i.id == s.to_string()));
    }

    #[tokio::test]
    async fn e2e_only_a_sessions_owner_and_viewers_see_it_and_only_its_owner_runs_it() {
        let runner = recorder(false);
        let Some(base) = start_recording(runner).await else {
            return;
        };
        let s = Uuid::new_v4();
        put(&base, s).await;
        let mine = |l: Vec<SessionInfo>| l.into_iter().find(|i| i.id == s.to_string());
        // another user sees nothing of it, and can't run it
        assert_eq!(mine(list(&base, B).await), None);
        for action in ["start", "stop"] {
            let path = format!("/sessions/{s}/{action}");
            assert_eq!(
                post_as(B, &base, &path, None).await,
                reqwest::StatusCode::NOT_FOUND
            );
        }
        // a viewer sees it, but can't run it either
        let viewer = format!("/sessions/{s}/viewers/{B}");
        let add = call(A, reqwest::Method::PUT, &base, &viewer)
            .send()
            .await
            .unwrap();
        assert_eq!(add.status(), reqwest::StatusCode::NO_CONTENT);
        let seen = mine(list(&base, B).await).unwrap();
        assert_eq!((seen.role, seen.owner.as_str()), (Role::Viewer, A));
        assert_eq!(mine(list(&base, A).await).unwrap().role, Role::Owner);
        let start = format!("/sessions/{s}/start");
        assert_eq!(
            post_as(B, &base, &start, None).await,
            reqwest::StatusCode::FORBIDDEN
        );
        // nor add viewers
        let more = format!("/sessions/{s}/viewers/user:c");
        let add = call(B, reqwest::Method::PUT, &base, &more)
            .send()
            .await
            .unwrap();
        assert_eq!(add.status(), reqwest::StatusCode::FORBIDDEN);
        assert_eq!(runner.of(s), []);
        // removed, it sees nothing again
        let gone = call(A, reqwest::Method::DELETE, &base, &viewer)
            .send()
            .await
            .unwrap();
        assert_eq!(gone.status(), reqwest::StatusCode::NO_CONTENT);
        assert_eq!(mine(list(&base, B).await), None);
        // and without saying who calls, nobody may
        let anon = reqwest::get(format!("http://{base}/sessions"))
            .await
            .unwrap();
        assert_eq!(anon.status(), reqwest::StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn e2e_a_lease_comes_with_credentials_sent_afresh_while_its_held() {
        let runner = recorder(false);
        let Some(base) =
            start_full(None, Duration::from_secs(2), false, None, None, |_| runner).await
        else {
            return;
        };
        let coder = Uuid::new_v4();
        put(&base, coder).await;
        let mut ws = connect(&base).await;
        let player = Uuid::new_v4().to_string();
        assert_eq!(acquire(&mut ws, coder, &player, None).await, acquired(1));
        let tokens = ws.tokens.clone().expect("credentials");
        let c = claims(&tokens.player);
        assert_eq!(
            (c.sub.as_str(), c.aud.as_slice(), c.iss.as_str()),
            (
                player_subject(&player).as_str(),
                &[CHLIVE.to_string()][..],
                "dirigent-control"
            )
        );
        assert_eq!(tokens.owner, None);
        // halfway through their life
        let fresh = Reply::Tokens {
            player: player_subject(&player),
            owner: None,
        };
        assert_eq!(next(&mut ws).await, fresh);
        assert_eq!(next(&mut ws).await, fresh);

        // a dirigent's act as its owner too, for chlive and control's API
        let dirigent = Uuid::new_v4();
        let body = serde_json::json!({ "part": "dirigent" });
        assert_eq!(
            put_json(&base, dirigent, A, body).await,
            reqwest::StatusCode::CREATED
        );
        let mut ws = connect(&base).await;
        let player = Uuid::new_v4().to_string();
        assert!(matches!(
            acquire(&mut ws, dirigent, &player, None).await,
            Reply::Acquired { .. }
        ));
        let owner = claims(&ws.tokens.clone().unwrap().owner.expect("an owner's"));
        assert_eq!(owner.sub, A);
        assert_eq!(owner.aud, [CHLIVE, CONTROL]);
        assert_eq!(owner.act.unwrap().sub, player_subject(&player));
        assert_eq!(
            next(&mut ws).await,
            Reply::Tokens {
                player: player_subject(&player),
                owner: Some(A.into()),
            }
        );
    }

    /// A request to the API with `token`.
    fn bearer(
        token: &str,
        method: reqwest::Method,
        base: &Ctl,
        path: &str,
    ) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .request(method, format!("http://{base}{path}"))
            .bearer_auth(token)
    }

    #[tokio::test]
    async fn e2e_in_oidc_auth_callers_are_who_the_providers_tokens_say() {
        use auth::test_provider::{Provider, CLIENT};
        let provider = Provider::new();
        let oidc = auth::Oidc::start(&provider.args()).await.unwrap();
        let runner = recorder(false);
        let Some(base) = start_full(
            Some(oidc),
            Duration::from_secs(600),
            false,
            None,
            None,
            |_| runner,
        )
        .await
        else {
            return;
        };
        let me = provider.id_token("me@x.com");
        let s = Uuid::new_v4();
        let put = bearer(&me, reqwest::Method::PUT, &base, &format!("/sessions/{s}"))
            .json(&serde_json::json!({ "part": "dirigent" }))
            .send()
            .await
            .unwrap();
        assert_eq!(put.status(), reqwest::StatusCode::CREATED);
        let listed: Vec<SessionInfo> = bearer(&me, reqwest::Method::GET, &base, "/sessions")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let mine = listed.iter().find(|i| i.id == s.to_string()).unwrap();
        assert_eq!(mine.owner, "user:me@x.com");

        // the header says nothing here; nor do the provider's tokens unverified, or for another
        let unverified = Provider::token_by(
            &provider.signer,
            serde_json::json!({ "aud": CLIENT, "email": "me@x.com", "email_verified": false }),
        );
        let elsewhere = Provider::token_by(
            &provider.signer,
            serde_json::json!({ "aud": "elsewhere", "email": "me@x.com", "email_verified": true }),
        );
        assert_eq!(
            call("user:me@x.com", reqwest::Method::GET, &base, "/sessions")
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        for t in [&unverified, &elsewhere] {
            let res = bearer(t, reqwest::Method::GET, &base, "/sessions")
                .send()
                .await
                .unwrap();
            assert_eq!(res.status(), reqwest::StatusCode::UNAUTHORIZED);
        }
        // chlive takes the provider's tokens: control signs none for callers
        let token = bearer(&me, reqwest::Method::POST, &base, "/token")
            .send()
            .await
            .unwrap();
        assert_eq!(token.status(), reqwest::StatusCode::NOT_FOUND);
        let mode: serde_json::Value = reqwest::get(format!("http://{base}/auth"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            mode,
            serde_json::json!({ "mode": "oidc", "google_client_id": CLIENT, "cli": null })
        );

        // the dirigent's player: as its owner, with the token for that; not with its own
        let mut ws = connect(&base).await;
        let player = Uuid::new_v4().to_string();
        assert!(matches!(
            acquire(&mut ws, s, &player, None).await,
            Reply::Acquired { .. }
        ));
        let tokens = ws.tokens.clone().unwrap();
        let res = bearer(&tokens.player, reqwest::Method::GET, &base, "/sessions")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::UNAUTHORIZED);
        let listed: Vec<SessionInfo> = bearer(
            &tokens.owner.unwrap(),
            reqwest::Method::GET,
            &base,
            "/sessions",
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
        assert!(listed.iter().any(|i| i.id == s.to_string()));
    }

    #[tokio::test]
    async fn e2e_in_declared_auth_control_signs_callers_chlive_credentials() {
        let Some(base) = start_recording(recorder(false)).await else {
            return;
        };
        let mode: serde_json::Value = reqwest::get(format!("http://{base}/auth"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(mode, serde_json::json!({ "mode": "declared" }));
        let got: serde_json::Value = call(A, reqwest::Method::POST, &base, "/token")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let token = got["token"].as_str().unwrap();
        // verified with the JWKS control publishes
        let jwks: jsonwebtoken::jwk::JwkSet =
            reqwest::get(format!("http://{base}/.well-known/jwks.json"))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
        let key = jsonwebtoken::DecodingKey::from_jwk(&jwks.keys[0]).unwrap();
        let mut v = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::EdDSA);
        v.set_audience(&[CHLIVE]);
        v.set_issuer(&["dirigent-control"]);
        let c = jsonwebtoken::decode::<token::Claims>(token, &key, &v)
            .unwrap()
            .claims;
        assert_eq!(c.sub, A);
        // for chlive only, not control's API
        let res = bearer(token, reqwest::Method::GET, &base, "/sessions")
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::UNAUTHORIZED);
        let anon = reqwest::Client::new()
            .post(format!("http://{base}/token"))
            .send()
            .await
            .unwrap();
        assert_eq!(anon.status(), reqwest::StatusCode::UNAUTHORIZED);
    }

    /// A control running mock players (`target/debug/player`, which `e2e` builds) in `root`, on
    /// the stack's chlive; its address.
    async fn start_local(root: &std::path::Path) -> Option<Ctl> {
        let root = root.to_path_buf();
        start_with(|ctl| {
            Box::leak(Box::new(Local {
                id: format!("local:{}", root.display()),
                root: root.clone(),
                program: concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/player").into(),
                args: vec!["--mock".into()],
                control_url: format!("http://{}", ctl.grpc),
                control_http: format!("http://{}", ctl.http),
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
        assert_eq!(
            put_json(&base, s, A, body).await,
            reqwest::StatusCode::CREATED
        );
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
    async fn e2e_a_local_player_stopped_from_outside_hands_the_session_over() {
        let root = tempfile::tempdir().unwrap();
        let Some(base) = start_local(root.path()).await else {
            return;
        };
        let s = Uuid::new_v4();
        let body = serde_json::json!({ "start": true });
        put_json(&base, s, A, body).await;
        let attached = |epoch| {
            move |i: &SessionInfo| i.epoch == epoch && i.player.as_ref().is_some_and(|p| p.attached)
        };
        info_until(&base, s, attached(1)).await;
        let pid = player_pid(s).expect("the player's process");
        // SAFETY: a plain kill(2)
        unsafe { libc::kill(pid, libc::SIGTERM) };
        info_until(&base, s, attached(2)).await;
        // handed over, not lost and started again past the grace period
        let log =
            std::fs::read_to_string(root.path().join(s.to_string()).join("player.log")).unwrap();
        assert!(log.contains("handing the session over"), "{log}");
        post(&base, &format!("/sessions/{s}/stop"), None).await;
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
        assert_eq!(
            put_json(&base, s, A, body).await,
            reqwest::StatusCode::CREATED
        );
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

    /// A fake OpenAI, and a control linking ChatGPT plans with it (a link key of its own).
    async fn start_linking(
        runner: &'static Recorder,
    ) -> Option<(Ctl, chatgpt::fake::OpenAi, tempfile::TempDir)> {
        std::env::var_os("DIRIGENT_E2E")?;
        let openai = chatgpt::fake::OpenAi::start().await;
        let keys = tempfile::tempdir().unwrap();
        seal::keygen(&keys.path().join("link.key")).unwrap();
        let args = openai.args(&keys.path().join("link.key"));
        let ctl = start_full(
            None,
            Duration::from_secs(600),
            false,
            Some(args),
            None,
            |_| runner,
        )
        .await?;
        Some((ctl, openai, keys))
    }

    /// A user of their own: links are kept by user, across the tests' runs.
    fn new_user() -> String {
        format!("user:plan-{}", Uuid::new_v4())
    }

    async fn get_json(
        base: &Ctl,
        user: &str,
        path: &str,
    ) -> (reqwest::StatusCode, serde_json::Value) {
        let res = call(user, reqwest::Method::GET, base, path)
            .send()
            .await
            .unwrap();
        let status = res.status();
        (status, res.json().await.unwrap_or_default())
    }

    /// `user` links the fake's plan, as the CLI does, its browser signing in at once.
    async fn link(base: &Ctl, user: &str) -> serde_json::Value {
        let res = call(user, reqwest::Method::POST, base, "/links/chatgpt/start")
            .json(&serde_json::json!({ "port": 1455 }))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::OK);
        let started: chatgpt::Started = res.json().await.unwrap();
        let browser = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let back = browser.get(&started.url).send().await.unwrap();
        let location = back.headers()["location"].to_str().unwrap();
        let back = reqwest::Url::parse(location).unwrap();
        assert_eq!(
            back.as_str().split('?').next(),
            Some("http://127.0.0.1:1455/auth/callback")
        );
        let query: HashMap<String, String> = back.query_pairs().into_owned().collect();
        assert_eq!(query["state"], started.state);
        let res = call(user, reqwest::Method::POST, base, "/links/chatgpt/finish")
            .json(&query)
            .send()
            .await
            .unwrap();
        assert_eq!(
            res.status(),
            reqwest::StatusCode::OK,
            "{:?}",
            res.text().await
        );
        res.json().await.unwrap()
    }

    /// The next credentials on the held lease's call, within 10 s.
    async fn next_tokens(conn: &mut Conn) -> pb::Tokens {
        let call = conn.call.as_mut().expect("a lease");
        match tokio::time::timeout(Duration::from_secs(10), call.rx.message()).await {
            Ok(Ok(Some(pb::ToPlayer {
                msg: Some(pb::to_player::Msg::Tokens(t)),
            }))) => t,
            other => panic!("no credentials: {other:?}"),
        }
    }

    #[tokio::test]
    async fn e2e_a_chatgpt_plan_is_linked_and_pays_for_its_owners_sessions() {
        let runner = recorder(false);
        let Some((base, openai, _keys)) = start_linking(runner).await else {
            return;
        };
        let user = new_user();
        let (_, me) = get_json(&base, &user, "/me").await;
        assert_eq!(
            me,
            serde_json::json!({
                "subject": user,
                "chatgpt": null,
                "anthropic": null,
            })
        );
        let plan = serde_json::json!({ "provider": "chatgpt", "model": "gpt-test" });
        let s = Uuid::new_v4();
        assert_eq!(
            put_json(&base, s, &user, plan.clone()).await,
            reqwest::StatusCode::UNPROCESSABLE_ENTITY
        );

        let linked = link(&base, &user).await;
        assert_eq!(linked["email"], chatgpt::fake::EMAIL);
        let (_, me) = get_json(&base, &user, "/me").await;
        assert_eq!(
            me["chatgpt"],
            serde_json::json!({ "email": chatgpt::fake::EMAIL, "broken": null })
        );
        let (status, models) = get_json(&base, &user, "/links/chatgpt/models").await;
        assert_eq!(status, reqwest::StatusCode::OK);
        assert_eq!(
            models,
            serde_json::json!([{ "slug": "gpt-test", "display_name": "GPT Test" }])
        );

        // a session on the plan: its player gets the plan's access token, and another once
        // OpenAI refuses that one
        assert_eq!(
            put_json(&base, s, &user, plan).await,
            reqwest::StatusCode::CREATED
        );
        let mut ws = connect(&base).await;
        let player = Uuid::new_v4().to_string();
        assert!(matches!(
            acquire(&mut ws, s, &player, None).await,
            Reply::Acquired { .. }
        ));
        let tokens = ws.tokens.clone().unwrap();
        let first = tokens.openai.clone().expect("an access token");
        assert_eq!(Some(&first), openai.state.access.lock().unwrap().as_ref());
        assert_eq!(tokens.openai_unavailable, None);
        let refresh = pb::FromPlayer {
            msg: Some(pb::from_player::Msg::RefreshTokens(pb::RefreshTokens {
                rejected: true,
            })),
        };
        ws.call.as_ref().unwrap().tx.send(refresh).await.unwrap();
        let renewed = next_tokens(&mut ws).await.openai.expect("an access token");
        assert_ne!(renewed, first);
        assert_eq!(Some(&renewed), openai.state.access.lock().unwrap().as_ref());
        // asked again without its being refused: the same one
        let again = pb::FromPlayer {
            msg: Some(pb::from_player::Msg::RefreshTokens(pb::RefreshTokens {
                rejected: false,
            })),
        };
        ws.call.as_ref().unwrap().tx.send(again).await.unwrap();
        assert_eq!(next_tokens(&mut ws).await.openai, Some(renewed));
        ws.close().await;

        // an Anthropic session's player gets none
        let other = Uuid::new_v4();
        assert_eq!(
            put_json(&base, other, &user, serde_json::json!({})).await,
            reqwest::StatusCode::CREATED
        );
        let mut ws = connect(&base).await;
        acquire(&mut ws, other, &Uuid::new_v4().to_string(), None).await;
        assert_eq!(ws.tokens.clone().unwrap().openai, None);
        ws.close().await;

        assert_eq!(
            call(&user, reqwest::Method::DELETE, &base, "/links/chatgpt")
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::NO_CONTENT
        );
        assert_eq!(openai.state.revoked.load(Ordering::SeqCst), 1);
        let (_, me) = get_json(&base, &user, "/me").await;
        assert_eq!(me["chatgpt"], serde_json::Value::Null);
        assert_eq!(
            call(&user, reqwest::Method::DELETE, &base, "/links/chatgpt")
                .send()
                .await
                .unwrap()
                .status(),
            reqwest::StatusCode::NOT_FOUND
        );
    }

    /// A control that keeps keys and secrets (with a link key of its own), checking keys with a
    /// fake Anthropic.
    async fn start_keyed() -> Option<(Ctl, secrets::fake::Anthropic, tempfile::TempDir)> {
        std::env::var_os("DIRIGENT_E2E")?;
        let anthropic = secrets::fake::Anthropic::start().await;
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("link.key");
        seal::keygen(&key).unwrap();
        let args = KeyArgs {
            anthropic_api: anthropic.url.clone(),
        };
        let keys = Some((args, Seal::load(&key).unwrap()));
        let runner = recorder(false);
        let ctl = start_full(None, Duration::from_secs(600), false, None, keys, |_| {
            runner
        })
        .await?;
        Some((ctl, anthropic, dir))
    }

    async fn send_json(
        base: &Ctl,
        user: &str,
        method: reqwest::Method,
        path: &str,
        body: serde_json::Value,
    ) -> (reqwest::StatusCode, String) {
        let res = call(user, method, base, path)
            .json(&body)
            .send()
            .await
            .unwrap();
        (res.status(), res.text().await.unwrap())
    }

    async fn delete(base: &Ctl, user: &str, path: &str) -> reqwest::StatusCode {
        let req = call(user, reqwest::Method::DELETE, base, path);
        req.send().await.unwrap().status()
    }

    #[tokio::test]
    async fn e2e_an_anthropic_key_is_checked_kept_and_sent_on_the_owners_leases() {
        let Some((base, _anthropic, _dir)) = start_keyed().await else {
            return;
        };
        let user = new_user();
        let (_, me) = get_json(&base, &user, "/me").await;
        assert_eq!(me["anthropic"], serde_json::Value::Null);
        let put = reqwest::Method::PUT;
        let (status, why) = send_json(
            &base,
            &user,
            put.clone(),
            "/links/anthropic",
            serde_json::json!({ "key": "sk-bad" }),
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::BAD_REQUEST);
        assert!(why.contains("invalid x-api-key"), "{why}");

        // without a key, the owner's sessions' leases say why there's none
        let s = Uuid::new_v4();
        let made = put_json(&base, s, &user, serde_json::json!({})).await;
        assert_eq!(made, reqwest::StatusCode::CREATED);
        let mut ws = connect(&base).await;
        acquire(&mut ws, s, &Uuid::new_v4().to_string(), None).await;
        let tokens = ws.tokens.clone().unwrap();
        assert_eq!(tokens.anthropic, None);
        let why = tokens.anthropic_unavailable.unwrap();
        assert!(why.contains("dirigent link anthropic"), "{why}");
        assert_eq!(tokens.openai, None);

        // theirs, once given, reaches the lease at once; and goes once removed
        let (status, hint) = send_json(
            &base,
            &user,
            put,
            "/links/anthropic",
            serde_json::json!({ "key": " sk-good-key-1234\n" }),
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK, "{hint}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&hint).unwrap(),
            serde_json::json!({ "hint": "1234" })
        );
        let key = next_tokens(&mut ws).await.anthropic;
        assert_eq!(key.as_deref(), Some("sk-good-key-1234"));
        let (_, me) = get_json(&base, &user, "/me").await;
        assert_eq!(me["anthropic"], serde_json::json!({ "hint": "1234" }));
        assert_eq!(
            delete(&base, &user, "/links/anthropic").await,
            reqwest::StatusCode::NO_CONTENT
        );
        let tokens = next_tokens(&mut ws).await;
        assert_eq!(tokens.anthropic, None);
        assert!(tokens.anthropic_unavailable.is_some());
        assert_eq!(
            delete(&base, &user, "/links/anthropic").await,
            reqwest::StatusCode::NOT_FOUND
        );
        ws.close().await;
    }

    #[tokio::test]
    async fn e2e_secrets_reach_their_sessions_leases_at_once_a_sessions_own_winning() {
        let Some((base, _anthropic, _dir)) = start_keyed().await else {
            return;
        };
        let user = new_user();
        let viewer = new_user();
        let (s, t) = (Uuid::new_v4(), Uuid::new_v4());
        let with_viewer = serde_json::json!({ "viewers": [viewer] });
        put_json(&base, s, &user, with_viewer).await;
        put_json(&base, t, &user, serde_json::json!({})).await;
        let put = |path: String, value: &str| {
            let (base, user) = (&base, user.clone());
            let body = serde_json::json!({ "value": value });
            async move {
                send_json(base, &user, reqwest::Method::PUT, &path, body)
                    .await
                    .0
            }
        };
        let no_content = reqwest::StatusCode::NO_CONTENT;
        assert_eq!(
            put("/secrets/GITHUB_TOKEN".into(), "everyone's").await,
            no_content
        );
        assert_eq!(
            put(format!("/sessions/{s}/secrets/GITHUB_TOKEN"), "s's").await,
            no_content
        );
        assert_eq!(
            put(format!("/sessions/{s}/secrets/NPM_TOKEN"), "npm").await,
            no_content
        );
        let bad = put(format!("/sessions/{s}/secrets/not-a-name"), "x").await;
        assert_eq!(bad, reqwest::StatusCode::BAD_REQUEST);
        let as_viewer = send_json(
            &base,
            &viewer,
            reqwest::Method::PUT,
            &format!("/sessions/{s}/secrets/MINE"),
            serde_json::json!({ "value": "x" }),
        )
        .await;
        assert_eq!(as_viewer.0, reqwest::StatusCode::FORBIDDEN);

        let secrets = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let mut on_s = connect(&base).await;
        acquire(&mut on_s, s, &Uuid::new_v4().to_string(), None).await;
        let got = on_s.tokens.clone().unwrap().secrets;
        assert_eq!(
            got,
            secrets(&[("GITHUB_TOKEN", "s's"), ("NPM_TOKEN", "npm")])
        );
        let mut on_t = connect(&base).await;
        acquire(&mut on_t, t, &Uuid::new_v4().to_string(), None).await;
        let got = on_t.tokens.clone().unwrap().secrets;
        assert_eq!(got, secrets(&[("GITHUB_TOKEN", "everyone's")]));

        // one for all the owner's sessions reaches both at once; one of s's own, only s
        assert_eq!(put("/secrets/EXTRA".into(), "x").await, no_content);
        let got = next_tokens(&mut on_s).await.secrets;
        let want = [
            ("EXTRA", "x"),
            ("GITHUB_TOKEN", "s's"),
            ("NPM_TOKEN", "npm"),
        ];
        assert_eq!(got, secrets(&want));
        let got = next_tokens(&mut on_t).await.secrets;
        assert_eq!(
            got,
            secrets(&[("EXTRA", "x"), ("GITHUB_TOKEN", "everyone's")])
        );
        let gone = delete(&base, &user, &format!("/sessions/{s}/secrets/GITHUB_TOKEN")).await;
        assert_eq!(gone, no_content);
        let got = next_tokens(&mut on_s).await.secrets;
        let want = [
            ("EXTRA", "x"),
            ("GITHUB_TOKEN", "everyone's"),
            ("NPM_TOKEN", "npm"),
        ];
        assert_eq!(got, secrets(&want));

        // the names, never the values, to whoever sees the session
        let (_, info) = get_json(&base, &viewer, &format!("/sessions/{s}")).await;
        assert_eq!(
            info["secrets"],
            serde_json::json!(["EXTRA", "GITHUB_TOKEN", "NPM_TOKEN"])
        );
        let (_, own) = get_json(&base, &user, &format!("/sessions/{s}/secrets")).await;
        assert_eq!(own, serde_json::json!(["NPM_TOKEN"]));
        let (_, all) = get_json(&base, &user, "/secrets").await;
        assert_eq!(all, serde_json::json!(["EXTRA", "GITHUB_TOKEN"]));
        on_s.close().await;
        on_t.close().await;
    }

    /// A ChatGpt of its own on the stack's Postgres, as a control just started would be: its
    /// access tokens not in memory yet. (Its store is never dropped: the test process would
    /// exit with its connection.)
    async fn restarted(
        openai: &chatgpt::fake::OpenAi,
        keys: &std::path::Path,
    ) -> (&'static ChatGpt, &'static Store) {
        let url = std::env::var("DIRIGENT_CONTROL_POSTGRES").unwrap();
        let store = Store::connect(&url, Duration::from_secs(1)).await.unwrap();
        let key = std::env::var("DIRIGENT_SIGNING_KEY").unwrap();
        let signer =
            Signer::load(key.as_ref(), "dirigent-control", Duration::from_secs(60)).unwrap();
        let chatgpt = ChatGpt::new(&openai.args(&keys.join("link.key")), signer.kid())
            .unwrap()
            .unwrap();
        (Box::leak(Box::new(chatgpt)), Box::leak(Box::new(store)))
    }

    #[tokio::test]
    async fn e2e_a_chatgpt_link_is_renewed_once_for_all_who_ask_and_breaks_when_refused() {
        let runner = recorder(false);
        let Some((base, openai, keys)) = start_linking(runner).await else {
            return;
        };
        let user = new_user();
        link(&base, &user).await;
        let (chatgpt, store) = restarted(&openai, keys.path()).await;
        let before = openai.state.refreshes.load(Ordering::SeqCst);
        let asks: Vec<_> = (0..10)
            .map(|_| {
                let user = user.clone();
                tokio::spawn(async move { chatgpt.access(store, &user, false).await })
            })
            .collect();
        let mut got = Vec::new();
        for ask in asks {
            got.push(ask.await.unwrap().expect("an access token"));
        }
        assert_eq!(openai.state.refreshes.load(Ordering::SeqCst), before + 1);
        assert!(got
            .iter()
            .all(|t| Some(t) == openai.state.access.lock().unwrap().as_ref()));

        // the refresh token it holds was used by someone else meanwhile: refused as reused, the
        // link is broken, and the plan pays for no new session until it's linked again
        *openai.state.refresh.lock().unwrap() = Some("rt-elsewhere".into());
        let (fresh, _) = restarted(&openai, keys.path()).await;
        let refused = fresh.access(store, &user, false).await;
        assert!(
            matches!(&refused, Err(chatgpt::Unavailable::Broken(why)) if why.contains("refresh_token_reused")),
            "{refused:?}"
        );
        let (_, me) = get_json(&base, &user, "/me").await;
        assert!(me["chatgpt"]["broken"]
            .as_str()
            .unwrap()
            .contains("refresh_token_reused"));
        let plan = serde_json::json!({ "provider": "chatgpt", "model": "gpt-test" });
        assert_eq!(
            put_json(&base, Uuid::new_v4(), &user, plan.clone()).await,
            reqwest::StatusCode::UNPROCESSABLE_ENTITY
        );
        link(&base, &user).await;
        let (_, me) = get_json(&base, &user, "/me").await;
        assert_eq!(me["chatgpt"]["broken"], serde_json::Value::Null);
        assert_eq!(
            put_json(&base, Uuid::new_v4(), &user, plan).await,
            reqwest::StatusCode::CREATED
        );
    }

    #[tokio::test]
    async fn e2e_a_chatgpt_sign_in_is_finished_only_by_who_started_it_and_once() {
        let runner = recorder(false);
        let Some((base, _openai, _keys)) = start_linking(runner).await else {
            return;
        };
        let user = new_user();
        let res = call(&user, reqwest::Method::POST, &base, "/links/chatgpt/start")
            .json(&serde_json::json!({ "port": 1455 }))
            .send()
            .await
            .unwrap();
        let started: chatgpt::Started = res.json().await.unwrap();
        let browser = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let back = browser.get(&started.url).send().await.unwrap();
        let back = reqwest::Url::parse(back.headers()["location"].to_str().unwrap()).unwrap();
        let query: HashMap<String, String> = back.query_pairs().into_owned().collect();
        let finish = |who: &str| {
            call(who, reqwest::Method::POST, &base, "/links/chatgpt/finish").json(&query)
        };
        assert_eq!(
            finish(&new_user()).send().await.unwrap().status(),
            reqwest::StatusCode::BAD_REQUEST
        );
        // taken by the attempt: not even its starter finishes it now
        assert_eq!(
            finish(&user).send().await.unwrap().status(),
            reqwest::StatusCode::BAD_REQUEST
        );
        let (_, me) = get_json(&base, &user, "/me").await;
        assert_eq!(me["chatgpt"], serde_json::Value::Null);
    }

    #[tokio::test]
    async fn e2e_the_cli_gives_an_anthropic_key_and_keeps_secrets_from_stdin() {
        let Some((base, _anthropic, _dir)) = start_keyed().await else {
            return;
        };
        let user = new_user();
        let home = tempfile::tempdir().unwrap();
        let creds = home.path().join("credentials.json");
        // the CLI with `args`, `input` on its stdin: what it printed, if it succeeded
        let cli = |args: &[&str], input: &str| {
            let mut c = tokio::process::Command::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../target/debug/dirigent"
            ));
            c.env("DIRIGENT_CREDENTIALS", &creds)
                .args(args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true);
            let input = input.to_string();
            async move {
                use tokio::io::AsyncWriteExt;
                let mut child = c.spawn().expect("the CLI (`e2e` builds it)");
                let mut stdin = child.stdin.take().unwrap();
                stdin.write_all(input.as_bytes()).await.unwrap();
                drop(stdin);
                let out = child.wait_with_output().await.unwrap();
                let printed = String::from_utf8_lossy(&out.stdout).trim().to_string();
                let failed = String::from_utf8_lossy(&out.stderr).trim().to_string();
                match out.status.success() {
                    true => Ok(printed),
                    false => Err(failed),
                }
            }
        };
        let url = format!("http://{base}");
        cli(&["login", "--control", &url, "--as", &user], "")
            .await
            .unwrap();
        let whoami = cli(&["whoami"], "").await.unwrap();
        assert!(whoami.contains("Anthropic: no key"), "{whoami}");
        let refused = cli(&["link", "anthropic"], "sk-bad\n").await.unwrap_err();
        assert!(refused.contains("invalid x-api-key"), "{refused}");
        let linked = cli(&["link", "anthropic"], "sk-good-cli-9876\n")
            .await
            .unwrap();
        assert_eq!(
            linked,
            "Anthropic: your key …9876 (your sessions run on it)"
        );
        let whoami = cli(&["whoami"], "").await.unwrap();
        assert!(whoami.contains("Anthropic: your key …9876"), "{whoami}");

        let s = Uuid::new_v4();
        put_json(&base, s, &user, serde_json::json!({})).await;
        let session = s.to_string();
        assert_eq!(
            cli(&["secret", "set", "GITHUB_TOKEN"], "ghp_x\n")
                .await
                .unwrap(),
            "Set GITHUB_TOKEN"
        );
        let set = cli(
            &["secret", "set", "NPM_TOKEN", "--session", &session],
            "n\n",
        );
        assert_eq!(set.await.unwrap(), "Set NPM_TOKEN");
        assert_eq!(cli(&["secret", "list"], "").await.unwrap(), "GITHUB_TOKEN");
        let own = cli(&["secret", "list", "--session", &session], "").await;
        assert_eq!(own.unwrap(), "NPM_TOKEN");
        let (_, info) = get_json(&base, &user, &format!("/sessions/{s}")).await;
        assert_eq!(
            info["secrets"],
            serde_json::json!(["GITHUB_TOKEN", "NPM_TOKEN"])
        );
        let rm = cli(&["secret", "rm", "GITHUB_TOKEN"], "").await;
        assert_eq!(rm.unwrap(), "Removed GITHUB_TOKEN");
        assert_eq!(cli(&["secret", "list"], "").await.unwrap(), "");
        let bad = cli(&["secret", "set", "lower"], "x\n").await.unwrap_err();
        assert!(bad.contains("isn't a secret's name"), "{bad}");
        assert_eq!(
            cli(&["unlink", "anthropic"], "").await.unwrap(),
            "Removed your Anthropic key"
        );
    }

    #[tokio::test]
    async fn e2e_the_cli_links_a_chatgpt_plan_through_its_loopback() {
        let runner = recorder(false);
        let Some((base, openai, _keys)) = start_linking(runner).await else {
            return;
        };
        let user = new_user();
        let home = tempfile::tempdir().unwrap();
        let creds = home.path().join("credentials.json");
        let cli = || {
            let mut c = tokio::process::Command::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../target/debug/dirigent"
            ));
            c.env("DIRIGENT_CREDENTIALS", &creds).kill_on_drop(true);
            c
        };
        let login = cli()
            .args([
                "login",
                "--control",
                &format!("http://{base}"),
                "--as",
                &user,
            ])
            .output()
            .await
            .expect("the CLI (`e2e` builds it)");
        assert!(login.status.success(), "{login:?}");
        assert_eq!(
            String::from_utf8_lossy(&login.stdout).trim(),
            format!("Signed in as {user}")
        );

        let mut linking = cli()
            .args(["link", "chatgpt", "--no-browser"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // the browser: the address it prints, followed to its loopback
        let mut stderr = tokio::io::BufReader::new(linking.stderr.take().unwrap());
        let url = tokio::time::timeout(Duration::from_secs(10), async {
            use tokio::io::AsyncBufReadExt;
            let mut line = String::new();
            loop {
                line.clear();
                assert!(stderr.read_line(&mut line).await.unwrap() > 0, "no address");
                if let Some(url) = line.trim().strip_prefix(&openai.url) {
                    return format!("{}{url}", openai.url);
                }
            }
        })
        .await
        .unwrap();
        let page = reqwest::get(&url).await.unwrap().text().await.unwrap();
        assert!(page.contains("ChatGPT is linked"), "{page}");
        let out = tokio::time::timeout(Duration::from_secs(10), linking.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            format!("Linked ChatGPT: {}", chatgpt::fake::EMAIL)
        );
        let whoami = cli().arg("whoami").output().await.unwrap();
        let whoami = String::from_utf8_lossy(&whoami.stdout);
        assert!(
            whoami.contains(&format!("ChatGPT: {}", chatgpt::fake::EMAIL)),
            "{whoami}"
        );
        let models = cli().arg("models").output().await.unwrap();
        assert_eq!(
            String::from_utf8_lossy(&models.stdout),
            "gpt-test\tGPT Test\n"
        );
        assert!(cli()
            .args(["unlink", "chatgpt"])
            .status()
            .await
            .unwrap()
            .success());
        assert!(cli().arg("logout").status().await.unwrap().success());
        assert!(!creds.exists());
    }
}
