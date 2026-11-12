//! dirigent's control plane, the only writer of its Postgres. Sessions are made, started,
//! stopped and read over HTTP (`/sessions`); players hold them over gRPC (`Players.Lease`, see
//! `protocol/proto`), each call being a lease, its connection's keepalives its liveness. A session with
//! a lease is active, one without idle: starting one, control reserves the lease and starts a
//! player for it (`runner`); stopping one, it tells the player to detach. A player it started
//! that's lost without giving the lease back is started again once the lease expires.

mod auth;
mod bootstrap;
mod chatgpt;
mod github;
mod leases;
mod machines;
mod nixstore;
mod pool;
mod prune;
mod runner;
mod seal;
mod secrets;
mod settings;
mod signin;
mod system;
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
    pb, player_subject, Part, Provider, Resources, Role, SessionInfo, SessionSpec, Source,
};
use serde::Deserialize;
use tokio::sync::{broadcast, mpsc, watch, Mutex};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::{Request, Response, Status, Streaming};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::chatgpt::{ChatGpt, ChatGptArgs, LinkInfo, Model, Started as LinkStarted};
use crate::github::{Github, GithubArgs, Repo, GITHUB_TOKEN};
use crate::leases::{Acquire, Archive, Delete, Put, Reserve, Scope, Store};
use crate::machines::{MachineArgs, Machines, Partial};
use crate::prune::{PruneArgs, Pruner};
use crate::runner::{Kube, Local, Runner};
use crate::seal::Seal;
use crate::secrets::{KeyArgs, Keys};
use crate::system::System;
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

    /// Build the cluster's own system for the players' VMs here, from a template of its own (JSON;
    /// Helm's): the player's image in build mode, with the configuration's ConfigMap
    /// (`--nixos-configmap`) and the cache's signing key as disks.
    #[arg(long, env = "DIRIGENT_KUBE_BUILD_TEMPLATE", requires = "kube_template")]
    kube_build_template: Option<PathBuf>,

    /// Write the shared Nix store's generations with Jobs from this template (JSON; Helm's), on
    /// the volume at `--nix-store`.
    #[arg(long, env = "DIRIGENT_KUBE_WRITER_TEMPLATE", requires_all = ["kube_template", "dir"])]
    kube_writer_template: Option<PathBuf>,

    #[command(flatten)]
    nix_store: nixstore::NixStoreArgs,

    #[command(flatten)]
    machines: MachineArgs,

    /// The configuration's ConfigMap, which control tells of its builds on (Events).
    #[arg(
        long,
        env = "DIRIGENT_NIXOS_CONFIGMAP",
        default_value = "dirigent-player-nixos"
    )]
    nixos_configmap: String,

    /// The cluster's own NixOS configuration for the players' VMs (`configuration.nix` and the
    /// files it imports): its system is built at start, unless it was already, for this
    /// configuration and version. Empty or absent: the stock image's.
    #[arg(long, env = "DIRIGENT_NIXOS_CONFIG")]
    nixos_config: Option<PathBuf>,

    /// How long building it may take, in seconds.
    #[arg(long, env = "DIRIGENT_NIXOS_BUILD_TIMEOUT", default_value_t = 3600)]
    nixos_build_timeout: u64,

    /// With local players, what builds it (nix/vm/build.nix's `dirigent-build`), here.
    #[arg(long, env = "DIRIGENT_PLAYER_BUILD")]
    player_build: Option<PathBuf>,

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

    #[command(flatten)]
    prune: PruneArgs,

    #[command(flatten)]
    github: GithubArgs,

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

    /// How long a web sign-in lasts unused, in days (each use starts it again).
    #[arg(long, env = "DIRIGENT_SESSION_DAYS", default_value_t = 30)]
    session_days: u64,

    /// More arguments for each player (e.g. `-- --mock`).
    #[arg(last = true)]
    player_args: Vec<String>,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Make a signing key in `dir` (`key.pem`), unless it has one, and write its public half
    /// (`jwks.json`, chlive's `jwks_file`).
    Keygen { dir: PathBuf },
    /// Make the chart's Secrets, those that are missing, in the service account's namespace;
    /// fill in the keys they lack, and change no value (control's first init container).
    EnsureSecrets {
        /// The chart's Garage's too.
        #[arg(long)]
        garage: bool,
        /// Write control's signing and link keys here too (`key.pem`, `link.key`): its pod can't
        /// mount their Secrets, which don't exist yet when its volumes are mounted.
        #[arg(long)]
        keys: Option<PathBuf>,
    },
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
    /// GitHub, for sessions on its repositories (and its App's tokens for them, if it has one).
    github: Github,
    /// What changed of leases' credentials, which their holders send again at once.
    changed: broadcast::Sender<Changed>,
    runner: Option<&'static dyn Runner>,
    /// The players' machines: the default one, the limits, and whose defaults they are.
    machines: Machines,
    /// The warm VMs' players waiting for a session.
    pool: pool::Pool,
    /// What deletes sessions; none without ClickHouse (`--clickhouse-url`).
    pruner: Option<&'static Pruner>,
    /// The cluster's own players' system, when it has a configuration of its own.
    system: Option<System>,
    startup: Duration,
    /// Whether only the players it started get leases (`--reserved-only`).
    reserved_only: bool,
    /// The connection holding each session's lease. Held across taking a lease and noting its
    /// connection here, and across a stop's looking here and dropping the lease: a stop either
    /// finds the connection or drops a lease no one is connected for.
    attached: Mutex<HashMap<Uuid, Attached>>,
    connections: AtomicU64,
    /// How long a web sign-in lasts unused (`--session-days`).
    sign_in_ttl: Duration,
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
    match &cli.command {
        Some(Command::Keygen { dir }) => return token::keygen(dir),
        Some(Command::EnsureSecrets { garage, keys }) => {
            return bootstrap::ensure(*garage, keys.as_deref()).await
        }
        None => {}
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
    let github = Github::new(&cli.github)?;
    let oidc = match cli.auth {
        AuthMode::Declared => None,
        AuthMode::Oidc => Some(auth::Oidc::start(&cli.oidc).await?),
    };
    let store = Store::connect(postgres, Duration::from_secs(cli.grace)).await?;
    let detached = store.detach_all().await?;
    let kube: Option<&'static Kube> = match &cli.kube_template {
        Some(template) => Some(Box::leak(Box::new(
            Kube::new(
                &cli.kube_name,
                cli.kube_namespace.as_deref(),
                template,
                cli.kube_build_template
                    .as_deref()
                    .map(|t| (t, cli.nixos_configmap.as_str())),
                cli.kube_writer_template.as_deref(),
            )
            .await?,
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
                build: cli.player_build.clone(),
            })))
        }
        None => None,
    };
    let args = &cli.machines;
    let defaults = runner.and_then(|r| r.machine()).unwrap_or(Resources {
        cpu: args.machine_cpu,
        memory: args.machine_memory,
        disk: args.machine_disk,
    });
    let machines = Machines::new(args, defaults)?;
    let pruner: Option<&'static Pruner> = Pruner::new(&cli.prune).map(|p| &*Box::leak(Box::new(p)));
    let system = match &cli.nixos_config {
        Some(dir) => System::load(dir, Duration::from_secs(cli.nixos_build_timeout))?,
        None => None,
    };
    let control = control(
        store,
        Auth { signer, oidc },
        chatgpt,
        keys,
        github,
        runner,
        machines,
        pruner,
        system,
        Duration::from_secs(cli.startup),
        cli.reserved_only,
        Duration::from_secs(cli.session_days * 86400),
    );
    tokio::spawn(sweep(control));
    // the warm VMs in the system it has, once that's known
    tokio::spawn(async move {
        system::begin(control).await;
        pool::keep(control).await
    });
    if let Some(pruner) = pruner {
        tokio::spawn(prune::prune(&control.store, runner, pruner));
        tokio::spawn(prune::maintain(pruner));
    }
    if let Some(kube) = kube {
        tokio::spawn(kube.clear(&control.store, control.startup, Duration::from_secs(30)));
        if cli.kube_writer_template.is_some() {
            tokio::spawn(nixstore::keep(control, kube, &cli.nix_store));
        }
    }
    let listener = tokio::net::TcpListener::bind(cli.listen).await?;
    let grpc = tokio::net::TcpListener::bind(cli.grpc_listen).await?;
    info!(
        listen = %cli.listen,
        grpc = %cli.grpc_listen,
        grace = cli.grace,
        runner = runner.map(|r| r.id()),
        auth = ?cli.auth,
        deletes = pruner.is_some(),
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

#[allow(clippy::too_many_arguments)]
fn control(
    store: Store,
    auth: Auth,
    chatgpt: Option<ChatGpt>,
    keys: Keys,
    github: Github,
    runner: Option<&'static dyn Runner>,
    machines: Machines,
    pruner: Option<&'static Pruner>,
    system: Option<System>,
    startup: Duration,
    reserved_only: bool,
    sign_in_ttl: Duration,
) -> &'static Control {
    Box::leak(Box::new(Control {
        store,
        auth,
        chatgpt,
        keys,
        github,
        changed: broadcast::channel(64).0,
        runner,
        machines,
        pool: Default::default(),
        pruner,
        system,
        startup,
        reserved_only,
        attached: Default::default(),
        connections: AtomicU64::new(0),
        sign_in_ttl,
    }))
}

fn router(control: &'static Control) -> Router {
    Router::new()
        .route("/sessions", get(list_sessions))
        .route(
            "/sessions/{id}",
            get(get_session).put(put_session).delete(delete_session),
        )
        .route("/sessions/{id}/start", post(start_session))
        .route("/sessions/{id}/stop", post(stop_session))
        .route("/sessions/{id}/archive", post(archive_session))
        .route("/sessions/{id}/unarchive", post(unarchive_session))
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
        .route("/links/github", get(github_link).delete(unlink_github))
        .route("/links/github/start", post(start_github))
        .route("/links/github/callback", get(github_callback))
        .route("/github/access", get(github_access))
        .route("/system", get(get_system))
        .route("/system/builds/{id}", post(system_built))
        .route("/nix/paths", post(nix_paths))
        .route("/settings", get(get_settings).put(put_settings))
        .route("/machines", get(get_machines))
        .route("/session", post(open_sign_in).delete(close_sign_in))
        .route("/session/token", post(sign_in_token))
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

/// Whether there's a GitHub App, and the accounts the caller connected its installations on:
/// `{"app", "accounts"}`.
async fn github_link(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
) -> Result<Json<serde_json::Value>, Failed> {
    let installations = control
        .store
        .github_installations(&caller)
        .await
        .map_err(|e| failed("reading the GitHub installations", e))?;
    let accounts: Vec<String> = installations.into_iter().map(|(_, a)| a).collect();
    Ok(Json(serde_json::json!({
        "app": control.github.has_app(),
        "accounts": accounts,
    })))
}

async fn unlink_github(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
) -> Result<StatusCode, Failed> {
    match control.store.unlink_github(&caller).await {
        Ok(true) => {
            let _ = control.changed.send(Changed::Owner(caller));
            Ok(StatusCode::NO_CONTENT)
        }
        Ok(false) => Err((StatusCode::NOT_FOUND, "no GitHub connected".into())),
        Err(e) => Err(failed("forgetting the GitHub installations", e)),
    }
}

#[derive(Deserialize)]
struct StartGithub {
    /// Authorizing the App for installations made already, rather than installing it.
    #[serde(default)]
    existing: bool,
}

/// Where the caller's browser goes to connect GitHub: `{"url"}`, GitHub's, which sends it back
/// to `/links/github/callback`; with the cookie that ties it to this browser.
async fn start_github(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
    Json(body): Json<StartGithub>,
) -> Result<axum::response::Response, Failed> {
    use axum::response::IntoResponse;
    let nonce = github::nonce();
    let audience = github::link_audience(&nonce);
    let state = control.auth.signer.sign(&caller, &[&audience], None);
    let url = control.github.connect_url(&state, body.existing).ok_or((
        StatusCode::NOT_FOUND,
        "this control has no GitHub App".into(),
    ))?;
    let cookie = [(axum::http::header::SET_COOKIE, github::set_cookie(&nonce))];
    Ok((cookie, Json(serde_json::json!({ "url": url }))).into_response())
}

/// What GitHub sends the browser back with.
#[derive(Deserialize)]
struct GithubBack {
    code: Option<String>,
    state: Option<String>,
}

/// GitHub sends the browser here once the App is installed or authorized: the installations
/// the GitHub user reaches become the caller's (who `state` says, as this browser's cookie
/// confirms: a navigation from GitHub's site has no sign-in cookie), and the browser goes on to
/// the web, `?github=connected` or `?github_error=<why>`.
async fn github_callback(
    State(control): State<&'static Control>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(back): axum::extract::Query<GithubBack>,
) -> axum::response::Response {
    use axum::response::{IntoResponse, Redirect};
    let linked = async {
        let nonce = github::from_cookies(cookies(&headers))
            .ok_or("this browser didn't start connecting GitHub (or took over ten minutes)")?;
        let claims = back
            .state
            .as_deref()
            .and_then(|s| {
                let audience = github::link_audience(nonce);
                control.auth.signer.verify(s, &audience).ok()
            })
            .ok_or("connecting GitHub took over ten minutes, or wasn't started here")?;
        let code = back.code.as_deref().ok_or(
            "GitHub sent no authorization: if an organization's owner has to approve the \
             App, connect again once they have",
        )?;
        let found = control.github.installations(code).await?;
        control
            .store
            .link_github(&claims.sub, &found)
            .await
            .map_err(|e| format!("keeping the GitHub installations: {e:#}"))?;
        info!(
            subject = claims.sub,
            installations = found.len(),
            "GitHub connected"
        );
        let _ = control.changed.send(Changed::Owner(claims.sub));
        Ok::<_, String>(())
    }
    .await;
    let to = match linked {
        Ok(()) => "/?github=connected".to_string(),
        Err(why) => {
            warn!("connecting GitHub: {why}");
            let mut url = reqwest::Url::parse("http://web/").expect("a URL");
            url.query_pairs_mut().append_pair("github_error", &why);
            format!("/?{}", url.query().unwrap_or_default())
        }
    };
    let cookie = [(axum::http::header::SET_COOKIE, github::clear_cookie())];
    (cookie, Redirect::to(&to)).into_response()
}

#[derive(Deserialize)]
struct AccessQuery {
    url: String,
}

/// Whether the caller's sessions can read the repository at `url`, and how: `{"repo": "o/r"
/// (none: not on GitHub), "via": "public" | "token" | "app" | none, "why", "app"}`, `why`
/// saying what GitHub refused, and `app` whether there's a GitHub App to connect.
async fn github_access(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
    axum::extract::Query(q): axum::extract::Query<AccessQuery>,
) -> Result<Json<serde_json::Value>, Failed> {
    let github = &control.github;
    let Some(repo) = Repo::parse(&q.url) else {
        return Ok(Json(
            serde_json::json!({ "repo": null, "app": github.has_app() }),
        ));
    };
    let mut why: Option<String> = None;
    let via = 'via: {
        match github.readable(&repo, None).await {
            Ok(true) => break 'via Some("public"),
            Ok(false) => {}
            Err(e) => warn!(
                repo = repo.full_name(),
                "asking GitHub about a repository: {e:#}"
            ),
        }
        let token = match &control.keys.seal {
            Some(seal) => match control.store.user_secret(&caller, GITHUB_TOKEN).await {
                Ok(Some(sealed)) => seal.open(&sealed).ok(),
                Ok(None) => None,
                Err(e) => return Err(failed("reading the GITHUB_TOKEN secret", e)),
            },
            None => None,
        };
        if let Some(token) = token {
            match github.readable(&repo, Some(&token)).await {
                Ok(true) => break 'via Some("token"),
                Ok(false) => {
                    why = Some(format!(
                        "GitHub refuses your GITHUB_TOKEN for {}: it may have expired, or not \
                         cover the repository",
                        repo.full_name()
                    ))
                }
                Err(e) => warn!(repo = repo.full_name(), "asking GitHub with a token: {e:#}"),
            }
        }
        match github.token(&control.store, &caller, &repo).await {
            Ok(Some(_)) => break 'via Some("app"),
            Ok(None) => {}
            Err(e) => why = Some(e),
        }
        None
    };
    Ok(Json(serde_json::json!({
        "repo": repo.full_name(),
        "via": via,
        "why": why,
        "app": github.has_app(),
    })))
}

/// How the cluster's own players' system goes (`player.nixos`): built, building, failed or
/// broken; or that it has none (the stock image).
async fn get_system(
    State(control): State<&'static Control>,
    Caller(_): Caller,
) -> Result<Json<system::Status>, Failed> {
    system::status(control)
        .await
        .map(Json)
        .map_err(|e| failed("reading the players' system", e))
}

/// A build VM tells how its build of the players' system went, with the build's token.
async fn system_built(
    State(control): State<&'static Control>,
    Path(id): Path<Uuid>,
    headers: axum::http::HeaderMap,
    Json(report): Json<system::Report>,
) -> Result<StatusCode, Failed> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or((StatusCode::UNAUTHORIZED, "no build token".to_string()))?;
    match system::built(control, id, token, &report).await {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err((
            StatusCode::NOT_FOUND,
            "no such build under way, with that token".into(),
        )),
        Err(e) => Err(failed("noting the players' system's build", e)),
    }
}

/// What a player's VM added to the shared Nix store, which the player tells with its own token.
#[derive(Deserialize)]
struct NixPaths {
    paths: Vec<nixstore::Added>,
}

/// A player tells what its VM added to the shared Nix store (with its token, chlive's).
async fn nix_paths(
    State(control): State<&'static Control>,
    headers: axum::http::HeaderMap,
    Json(body): Json<NixPaths>,
) -> Result<StatusCode, Failed> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or((StatusCode::UNAUTHORIZED, "no player's token".to_string()))?;
    let claims = control.auth.signer.verify(token, CHLIVE).map_err(|e| {
        (
            StatusCode::UNAUTHORIZED,
            format!("not a player's token: {e}"),
        )
    })?;
    if !claims.sub.starts_with("player:") {
        return Err((StatusCode::FORBIDDEN, "only players tell this".into()));
    }
    let paths: Vec<(String, bool)> = body
        .paths
        .into_iter()
        .filter(|a| a.path.starts_with("/nix/store/") && a.path.len() < 256)
        .take(10_000)
        .map(|a| (a.path, a.signed))
        .collect();
    control
        .store
        .nix_added(&paths)
        .await
        .map_err(|e| failed("noting the store paths a VM added", e))?;
    Ok(StatusCode::NO_CONTENT)
}

/// The players' machines: the default one, the most a session's may have, and whether users
/// have their own defaults.
async fn get_machines(
    State(control): State<&'static Control>,
    Caller(_): Caller,
) -> Json<&'static Machines> {
    Json(&control.machines)
}

/// The caller's settings, defaults for those they haven't set.
async fn get_settings(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
) -> Result<Json<settings::Settings>, Failed> {
    let kept = control
        .store
        .settings(&caller)
        .await
        .map_err(|e| failed("reading the settings", e))?;
    Ok(Json(kept.map(settings::Settings::kept).unwrap_or_default()))
}

/// The caller's settings are these now (all of them: those left out are back to their
/// defaults); what's unknown or wrong is refused.
async fn put_settings(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
    body: axum::body::Bytes,
) -> Result<Json<settings::Settings>, Failed> {
    let given: settings::Settings = serde_json::from_slice(&body)
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("not settings: {e}")))?;
    if let Some(machine) = given.machine {
        let machines = &control.machines;
        if !machines.per_user {
            return Err((
                StatusCode::BAD_REQUEST,
                "users have no default machines of their own here".into(),
            ));
        }
        if let Some(why) = machines.over(machine.over(machines.defaults)) {
            return Err((StatusCode::BAD_REQUEST, format!("not that machine: {why}")));
        }
    }
    let value = serde_json::to_value(&given).expect("settings are JSON");
    control
        .store
        .put_settings(&caller, &value)
        .await
        .map_err(|e| failed("keeping the settings", e))?;
    if control.machines.per_user {
        control.pool.wake();
    }
    Ok(Json(given))
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

/// A web sign-in's answer: who it is, and a token of control's for its API and chlive.
fn signed_in(subject: String, control: &Control, cookie: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    let token = control.auth.signer.sign(&subject, &[CONTROL, CHLIVE], None);
    (
        [(axum::http::header::SET_COOKIE, cookie)],
        Json(serde_json::json!({ "subject": subject, "token": token })),
    )
        .into_response()
}

fn no_sign_in(why: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        StatusCode::UNAUTHORIZED,
        [(axum::http::header::SET_COOKIE, signin::clear_cookie())],
        why.to_string(),
    )
        .into_response()
}

fn cookies(headers: &axum::http::HeaderMap) -> &str {
    headers
        .get(axum::http::header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// Sign in on the web (`oidc` auth), with the OpenID provider's ID token: a sign-in of
/// control's, held in a cookie (`signin`), and a first token.
async fn open_sign_in(
    State(control): State<&'static Control>,
    headers: axum::http::HeaderMap,
) -> Result<axum::response::Response, Failed> {
    if control.auth.oidc.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            "there are no sign-ins in declared auth".into(),
        ));
    }
    let subject = control
        .auth
        .provider_caller(&headers)
        .await
        .map_err(|why| (StatusCode::UNAUTHORIZED, why))?;
    let secret = signin::secret();
    let id = control
        .store
        .open_web_session(&subject, &secret, control.sign_in_ttl)
        .await
        .map_err(|e| failed("opening a sign-in", e))?;
    info!(subject, "signed in");
    let cookie = signin::set_cookie(id, &secret, control.sign_in_ttl.as_secs());
    Ok(signed_in(subject, control, cookie))
}

/// A fresh token for the web sign-in in the cookie, which lasts as long again from now.
async fn sign_in_token(
    State(control): State<&'static Control>,
    headers: axum::http::HeaderMap,
) -> Result<axum::response::Response, Failed> {
    if control.auth.oidc.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            "there are no sign-ins in declared auth".into(),
        ));
    }
    let Some((id, secret, value)) = signin::from_cookies(cookies(&headers)) else {
        return Ok(no_sign_in("not signed in"));
    };
    match control
        .store
        .web_session(id, &secret, control.sign_in_ttl)
        .await
    {
        Ok(Some(subject)) => {
            let cookie = signin::renew_cookie(value, control.sign_in_ttl.as_secs());
            Ok(signed_in(subject, control, cookie))
        }
        Ok(None) => Ok(no_sign_in("the sign-in is over")),
        Err(e) => Err(failed("reading a sign-in", e)),
    }
}

/// Sign out: the sign-in in the cookie is over.
async fn close_sign_in(
    State(control): State<&'static Control>,
    headers: axum::http::HeaderMap,
) -> Result<axum::response::Response, Failed> {
    use axum::response::IntoResponse;
    if let Some((id, _, _)) = signin::from_cookies(cookies(&headers)) {
        control
            .store
            .close_web_session(id)
            .await
            .map_err(|e| failed("ending a sign-in", e))?;
    }
    Ok((
        StatusCode::NO_CONTENT,
        [(axum::http::header::SET_COOKIE, signin::clear_cookie())],
    )
        .into_response())
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
            system::lost(control, session);
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
    /// It's archived: no player takes it until it's unarchived.
    Archived,
    /// There's no runner to start players with.
    NoRunner,
}

/// A lease not reserved, as a start's outcome.
impl From<Reserve> for Started {
    fn from(r: Reserve) -> Started {
        match r {
            Reserve::Reserved(epoch) => Started::Player(epoch),
            Reserve::Held => Started::Active,
            Reserve::Unknown => Started::Unknown,
            Reserve::Archived => Started::Archived,
        }
    }
}

/// Start a player for `session`, unless one holds it.
async fn start(control: &'static Control, session: Uuid) -> anyhow::Result<Started> {
    let Some(runner) = control.runner else {
        return Ok(Started::NoRunner);
    };
    // its own machine, if it has one (none: the default one, as it is now)
    let machine = control
        .store
        .machine(session)
        .await?
        .unwrap_or(control.machines.defaults);
    // a warm VM on it, if one waits; not for a suspect, which starts on the stock image
    let suspect = control.system.as_ref().is_some_and(|s| s.suspect(session));
    let warm = match pool::key(control, machine) {
        Some(key) if !suspect => control.pool.take(machine, &key),
        _ => None,
    };
    if let Some(warm) = warm {
        let subject = player_subject(&warm.player);
        let epoch = match control
            .store
            .reserve(session, &subject, runner.id(), control.startup)
            .await
        {
            Ok(Reserve::Reserved(epoch)) => epoch,
            not => {
                control.pool.give_back(warm);
                return not.map(Started::from);
            }
        };
        control.pool.wake();
        let player = warm.player.clone();
        let taken = match runner.take(session, &player, epoch).await {
            Ok(()) => warm.tell(session, epoch).await,
            Err(e) => {
                warn!(session = %session, player, "taking a warm VM: {e:#}");
                false
            }
        };
        if taken {
            return Ok(Started::Player(epoch));
        }
        // gone meanwhile: a VM of its own after all
        warn!(session = %session, player, "the warm VM was gone: starting another");
        runner.unwarm(&player).await;
        control.store.drop_lease(session).await?;
    }
    let player = Uuid::new_v4().to_string();
    let subject = player_subject(&player);
    let epoch = match control
        .store
        .reserve(session, &subject, runner.id(), control.startup)
        .await?
    {
        Reserve::Reserved(epoch) => epoch,
        not => return Ok(not.into()),
    };
    // the cluster's own system, once it's built, where the runner runs it
    let system = control
        .system
        .as_ref()
        .filter(|_| runner.runs_systems())
        .and_then(|s| s.start(session, epoch));
    if let Err(e) = runner
        .start(session, &player, epoch, system.as_deref(), machine)
        .await
    {
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
    /// Its machine, as asked for: what's left out is the owner's default (or the cluster's).
    #[serde(default)]
    resources: Option<Partial>,
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
    Json(mut body): Json<NewSession>,
) -> Result<StatusCode, Failed> {
    let machines = &control.machines;
    let own = match machines.per_user {
        true => control
            .store
            .settings(&caller)
            .await
            .map_err(|e| failed("reading the settings", e))?
            .and_then(|s| settings::Settings::kept(s).machine),
        false => None,
    };
    body.spec.resources = machines.resolve(body.resources, own);
    if let Some(why) = body.spec.resources.and_then(|r| machines.over(r)) {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("not that machine: {why}"),
        ));
    }
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
            // perhaps a user active anew, whose warm VMs there are
            if machines.per_user {
                control.pool.wake();
            }
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
        Ok(Started::Archived) => Err((StatusCode::CONFLICT, "the session is archived".into())),
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

/// Delete the session, the caller's and archived: no one has access from now on, and control
/// removes what it left (`prune`).
async fn delete_session(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Failed> {
    owned(control, id, &caller).await?;
    if control.pruner.is_none() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            "control can't delete sessions (no --clickhouse-url)".into(),
        ));
    }
    match control.store.delete(id).await {
        Ok(Delete::Marked) => {
            info!(session = %id, "session deleting");
            Ok(StatusCode::ACCEPTED)
        }
        Ok(Delete::NotArchived) => Err((
            StatusCode::CONFLICT,
            "only an archived session is deleted: archive it first".into(),
        )),
        Ok(Delete::Unknown) => Err(not_found()),
        Err(e) => Err(failed(&format!("deleting the session {id}"), e)),
    }
}

/// Archive the session, the caller's: it's listed as archived, and no player takes it until
/// it's unarchived. One a player holds is stopped first.
async fn archive_session(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Failed> {
    archived(control, id, &caller, true).await
}

async fn unarchive_session(
    State(control): State<&'static Control>,
    Caller(caller): Caller,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Failed> {
    archived(control, id, &caller, false).await
}

async fn archived(
    control: &'static Control,
    id: Uuid,
    caller: &str,
    on: bool,
) -> Result<StatusCode, Failed> {
    owned(control, id, caller).await?;
    match control.store.archive(id, on).await {
        Ok(Archive::Done) => {
            if on {
                info!(session = %id, "session archived");
            } else {
                info!(session = %id, "session unarchived");
            }
            Ok(StatusCode::NO_CONTENT)
        }
        Ok(Archive::Active) => Err((
            StatusCode::CONFLICT,
            "a player holds the session: stop it first".into(),
        )),
        Ok(Archive::Unknown) => Err(not_found()),
        Err(e) => Err(failed(&format!("archiving the session {id}"), e)),
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
    type WaitStream = ReceiverStream<Result<pb::Assigned, Status>>;

    /// A warm player waits to be given a session.
    async fn wait(
        &self,
        request: Request<pb::WaitRequest>,
    ) -> Result<Response<Self::WaitStream>, Status> {
        let ask = request.into_inner();
        let rx = pool::wait(self.0, ask.player, ask.system).await?;
        Ok(Response::new(ReceiverStream::new(rx)))
    }

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
            Ok(Acquire::Acquired(epoch, spec, owner)) => {
                // its VM came up (in the cluster's system, if it was started with it; on the stock
                // image, if it was started again so, which says the system is broken)
                if let Some(system) = &control.system {
                    if system.came(session, epoch) {
                        tokio::spawn(system::broken(control, session));
                    }
                }
                (epoch, spec, owner)
            }
            Ok(Acquire::Held) => {
                return Err(Status::failed_precondition(
                    "another player holds the session",
                ))
            }
            Ok(Acquire::Unknown) => return Err(Status::not_found("no such session")),
            Ok(Acquire::Archived) => {
                return Err(Status::failed_precondition("the session is archived"))
            }
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
            repo: match &spec.source {
                Some(Source::Git { url, .. }) => Repo::parse(url),
                None => None,
            },
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
/// install's; and the session's secrets, a GitHub repository's `GITHUB_TOKEN` from the App
/// unless it has its own.
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
    let mut secrets = match session_secrets(control, held.session).await {
        Ok(secrets) => secrets,
        Err(e) => {
            error!(session = %held.session, "reading the session's secrets: {e:#}");
            Default::default()
        }
    };
    if let (Some(repo), false) = (&held.repo, secrets.contains_key(GITHUB_TOKEN)) {
        match control.github.token(&control.store, owner, repo).await {
            Ok(Some(token)) => {
                secrets.insert(GITHUB_TOKEN.into(), token);
            }
            Ok(None) => {}
            Err(why) => warn!(session = %held.session, owner, "no GitHub App token: {why}"),
        }
    }
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
    /// Its repository, if it's on GitHub: the App's token for it is its `GITHUB_TOKEN`, unless it
    /// has one.
    repo: Option<Repo>,
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

    /// Notes the players it's to start: (session, player, epoch), and the system each is to
    /// start in; and the builds of the players' system (id, token), and those ended.
    struct Recorder {
        id: String,
        started: std::sync::Mutex<Vec<(Uuid, String, i64)>>,
        systems: std::sync::Mutex<Vec<(Uuid, Option<String>)>>,
        machines: std::sync::Mutex<Vec<(Uuid, Resources)>>,
        builds: std::sync::Mutex<Vec<(Uuid, String)>>,
        ended: std::sync::Mutex<Vec<Uuid>>,
        fail: bool,
        /// Whether it keeps warm VMs: those it has (made, and not taken or deleted), those
        /// taken (session, player, epoch), and what's in their keys besides (a generation).
        warms: bool,
        warm: std::sync::Mutex<Vec<runner::Warm>>,
        taken: std::sync::Mutex<Vec<(Uuid, String, i64)>>,
        generation: std::sync::atomic::AtomicU64,
    }

    #[async_trait::async_trait]
    impl Runner for Recorder {
        fn id(&self) -> &str {
            &self.id
        }

        async fn start(
            &self,
            session: Uuid,
            player: &str,
            epoch: i64,
            system: Option<&str>,
            machine: Resources,
        ) -> anyhow::Result<()> {
            anyhow::ensure!(!self.fail, "the runner fails");
            self.machines.lock().unwrap().push((session, machine));
            self.started
                .lock()
                .unwrap()
                .push((session, player.into(), epoch));
            self.systems
                .lock()
                .unwrap()
                .push((session, system.map(Into::into)));
            Ok(())
        }

        async fn forget(&self, _: Uuid) -> anyhow::Result<()> {
            Ok(())
        }

        async fn build(&self, id: Uuid, token: &str, _: &std::path::Path) -> anyhow::Result<()> {
            self.builds.lock().unwrap().push((id, token.into()));
            Ok(())
        }

        async fn end_build(&self, id: Uuid) {
            self.ended.lock().unwrap().push(id);
        }

        fn runs_systems(&self) -> bool {
            true
        }

        fn warms(&self) -> bool {
            self.warms
        }

        fn warm_key(&self, m: Resources, system: Option<&str>) -> String {
            let generation = self.generation.load(Ordering::Relaxed);
            format!("{}-{}-{}-{system:?}-{generation}", m.cpu, m.memory, m.disk)
        }

        async fn warm(
            &self,
            player: &str,
            machine: Resources,
            _system: Option<&str>,
            key: &str,
        ) -> anyhow::Result<()> {
            self.warm.lock().unwrap().push(runner::Warm {
                player: player.into(),
                machine,
                key: key.into(),
                age: Duration::ZERO,
            });
            Ok(())
        }

        async fn warm_vms(&self) -> anyhow::Result<Vec<runner::Warm>> {
            Ok(self.warm.lock().unwrap().clone())
        }

        async fn take(&self, session: Uuid, player: &str, epoch: i64) -> anyhow::Result<()> {
            self.warm.lock().unwrap().retain(|w| w.player != player);
            self.taken
                .lock()
                .unwrap()
                .push((session, player.into(), epoch));
            Ok(())
        }

        async fn unwarm(&self, player: &str) {
            self.warm.lock().unwrap().retain(|w| w.player != player);
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
        recorder_of(fail, false)
    }

    /// One that keeps warm VMs.
    fn warm_recorder() -> &'static Recorder {
        recorder_of(false, true)
    }

    fn recorder_of(fail: bool, warms: bool) -> &'static Recorder {
        Box::leak(Box::new(Recorder {
            id: format!("test:{}", Uuid::new_v4()),
            started: Default::default(),
            systems: Default::default(),
            machines: Default::default(),
            builds: Default::default(),
            ended: Default::default(),
            fail,
            warms,
            warm: Default::default(),
            taken: Default::default(),
            generation: Default::default(),
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
        let github = no_github();
        start_all(
            oidc,
            ttl,
            reserved_only,
            chatgpt,
            keys,
            github,
            STARTUP,
            None,
            None,
            runner,
        )
        .await
    }

    /// GitHub where there is none, with no App.
    fn no_github() -> GithubArgs {
        GithubArgs {
            github_api: "http://127.0.0.1:1".into(),
            github_web: "http://127.0.0.1:1".into(),
            github_app_slug: None,
            github_app_client_id: None,
            github_app_client_secret: None,
            github_app_key: None,
        }
    }

    /// The machines of a test's control: 2 cores, 4 GiB and 20 GiB, no limits, the cluster's
    /// defaults for everyone, and no warm VMs.
    fn test_machines() -> Machines {
        Machines {
            defaults: Resources {
                cpu: 2,
                memory: 4,
                disk: 20,
            },
            limits: Partial::default(),
            per_user: false,
            warm: 0,
            active_within: Duration::from_secs(604800),
        }
    }

    /// A started player's time to take its lease, where the runner only notes it.
    const STARTUP: Duration = Duration::from_secs(1);

    /// The same, with `github` for GitHub, `startup` for a started player to take its lease, and
    /// the cluster's own players' `system`, if it has one.
    #[allow(clippy::too_many_arguments)]
    async fn start_all(
        oidc: Option<Arc<auth::Oidc>>,
        ttl: Duration,
        reserved_only: bool,
        chatgpt: Option<ChatGptArgs>,
        keys: Option<(KeyArgs, Seal)>,
        github: GithubArgs,
        startup: Duration,
        system: Option<System>,
        machines: Option<Machines>,
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
        let github = Github::new(&github).unwrap();
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
        let runner = runner(&ctl);
        // deleting, with the stack's ClickHouse (the `e2e` script's), events settled at once
        let pruner: Option<&'static Pruner> = Pruner::new(&PruneArgs {
            clickhouse_url: std::env::var("DIRIGENT_CLICKHOUSE_URL").ok(),
            clickhouse_user: "dirigent_control".into(),
            clickhouse_password: "dirigent_control".into(),
            kopia_token: None,
            kopia_token_secret: None,
            kopia_state: None,
        })
        .map(|mut p| {
            p.settled = Duration::ZERO;
            &*Box::leak(Box::new(p))
        });
        let control = control(
            store,
            Auth { signer, oidc },
            chatgpt,
            keys,
            github,
            Some(runner),
            machines.unwrap_or_else(test_machines),
            pruner,
            system,
            startup,
            reserved_only,
            Duration::from_secs(3600),
        );
        tokio::spawn(sweep(control));
        tokio::spawn(async move {
            system::begin(control).await;
            pool::keep(control).await
        });
        if let Some(pruner) = pruner {
            tokio::spawn(prune::prune(&control.store, Some(runner), pruner));
        }
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
    async fn e2e_an_archived_session_takes_no_player_until_unarchived() {
        let runner = recorder(false);
        let Some(base) = start_recording(runner).await else {
            return;
        };
        let s = Uuid::new_v4();
        put(&base, s).await;
        let mut a = connect(&base).await;
        assert_eq!(acquire(&mut a, s, "a", None).await, acquired(1));
        let (archive, unarchive) = (
            format!("/sessions/{s}/archive"),
            format!("/sessions/{s}/unarchive"),
        );
        let start = format!("/sessions/{s}/start");
        assert_eq!(
            post(&base, &archive, None).await,
            reqwest::StatusCode::CONFLICT
        );
        a.close().await;
        info_until(&base, s, |i| i.player.is_none()).await;
        assert_eq!(
            post(&base, &archive, None).await,
            reqwest::StatusCode::NO_CONTENT
        );
        assert!(info(&base, s).await.archived.is_some());
        assert_eq!(
            post(&base, &start, None).await,
            reqwest::StatusCode::CONFLICT
        );
        let mut b = connect(&base).await;
        assert_eq!(acquire(&mut b, s, "b", None).await, HELD);
        assert!(runner.of(s).is_empty());
        let as_b = post_as(B, &base, &unarchive, None).await;
        assert_eq!(as_b, reqwest::StatusCode::NOT_FOUND);
        assert_eq!(
            post(&base, &unarchive, None).await,
            reqwest::StatusCode::NO_CONTENT
        );
        assert_eq!(info(&base, s).await.archived, None);
        assert_eq!(acquire(&mut b, s, "b", None).await, acquired(2));
    }

    /// A row of `session`'s log, put straight into the stack's ClickHouse (as the sink would).
    async fn event_row(session: Uuid) {
        let url = std::env::var("DIRIGENT_CLICKHOUSE_URL").unwrap();
        let sql = format!(
            "INSERT INTO dirigent.session_events (session_id, offset, partition, ts, id, author, \
             kind, data) VALUES ('{session}', 0, 0, now64(3), '{}', 'user:a', 'user_message', \
             '{{\"text\":\"hi\"}}')",
            Uuid::new_v4()
        );
        let res = reqwest::Client::new()
            .post(url)
            .body(sql)
            .send()
            .await
            .unwrap();
        assert!(res.status().is_success(), "{}", res.text().await.unwrap());
    }

    /// How many rows `session` has in ClickHouse, and whether Postgres has it still.
    async fn left_of(session: Uuid) -> (u64, bool) {
        let url = std::env::var("DIRIGENT_CLICKHOUSE_URL").unwrap();
        let sql =
            format!("SELECT count() FROM dirigent.session_events WHERE session_id = '{session}'");
        let res = reqwest::Client::new()
            .post(url)
            .body(sql)
            .send()
            .await
            .unwrap();
        let rows = res.text().await.unwrap().trim().parse().unwrap();
        let pg = std::env::var("DIRIGENT_CONTROL_POSTGRES").unwrap();
        let (db, conn) = tokio_postgres::connect(&pg, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(conn);
        let row = db
            .query_opt("SELECT 1 FROM dirigent.sessions WHERE id = $1", &[&session])
            .await
            .unwrap();
        (rows, row.is_some())
    }

    #[tokio::test]
    async fn e2e_a_deleted_session_goes_with_its_events() {
        let Some(base) = start_recording(recorder(false)).await else {
            return;
        };
        let s = Uuid::new_v4();
        put(&base, s).await;
        let viewer = call(
            A,
            reqwest::Method::PUT,
            &base,
            &format!("/sessions/{s}/viewers/{B}"),
        )
        .send()
        .await
        .unwrap();
        assert_eq!(viewer.status(), reqwest::StatusCode::NO_CONTENT);
        event_row(s).await;
        let path = format!("/sessions/{s}");
        let delete = |user| call(user, reqwest::Method::DELETE, &base, &path).send();
        assert_eq!(
            delete(A).await.unwrap().status(),
            reqwest::StatusCode::CONFLICT
        );
        assert_eq!(
            post(&base, &format!("/sessions/{s}/archive"), None).await,
            reqwest::StatusCode::NO_CONTENT
        );
        assert_eq!(
            delete(B).await.unwrap().status(),
            reqwest::StatusCode::FORBIDDEN
        );
        assert_eq!(
            delete(A).await.unwrap().status(),
            reqwest::StatusCode::ACCEPTED
        );
        // one that comes after its marking goes too
        event_row(s).await;
        for user in [A, B] {
            assert!(!list(&base, user)
                .await
                .iter()
                .any(|i| i.id == s.to_string()));
        }
        assert_eq!(
            delete(A).await.unwrap().status(),
            reqwest::StatusCode::NOT_FOUND
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            let left = left_of(s).await;
            if left == (0, false) {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "left: {left:?}");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
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

    /// The sign-in cookie's value in an answer's `Set-Cookie` (empty when it's cleared).
    fn sign_in_cookie(res: &reqwest::Response) -> String {
        let set = res.headers()[reqwest::header::SET_COOKIE].to_str().unwrap();
        let value = set.split(';').next().unwrap().split_once('=').unwrap().1;
        value.to_string()
    }

    #[tokio::test]
    async fn e2e_a_web_sign_in_gives_tokens_until_it_ends() {
        use auth::test_provider::Provider;
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
        let http = reqwest::Client::new();
        let refresh = |cookie: &str| {
            http.post(format!("http://{base}/session/token"))
                .header(
                    reqwest::header::COOKIE,
                    format!("{}={cookie}", signin::COOKIE),
                )
                .send()
        };
        let opened = bearer(
            &provider.id_token("me@x.com"),
            reqwest::Method::POST,
            &base,
            "/session",
        )
        .send()
        .await
        .unwrap();
        assert_eq!(opened.status(), reqwest::StatusCode::OK);
        let cookie = sign_in_cookie(&opened);
        let body: serde_json::Value = opened.json().await.unwrap();
        assert_eq!(body["subject"], "user:me@x.com");

        // its tokens are control's, for its API and chlive
        let again = refresh(&cookie).await.unwrap();
        assert_eq!(again.status(), reqwest::StatusCode::OK);
        assert_eq!(sign_in_cookie(&again), cookie);
        let token = again.json::<serde_json::Value>().await.unwrap()["token"]
            .as_str()
            .unwrap()
            .to_string();
        let minted = claims(&token);
        assert_eq!(
            (minted.sub.as_str(), minted.aud.clone()),
            (
                "user:me@x.com",
                vec![CONTROL.to_string(), CHLIVE.to_string()]
            )
        );
        let listed = bearer(&token, reqwest::Method::GET, &base, "/sessions")
            .send()
            .await
            .unwrap();
        assert_eq!(listed.status(), reqwest::StatusCode::OK);
        // which make no sign-ins of their own
        let by_minted = bearer(&token, reqwest::Method::POST, &base, "/session")
            .send()
            .await
            .unwrap();
        assert_eq!(by_minted.status(), reqwest::StatusCode::UNAUTHORIZED);

        // a wrong secret is no sign-in, nor is one expired
        let (id, _) = cookie.split_once('.').unwrap();
        let wrong = format!("{id}.{}", "A".repeat(43));
        let res = refresh(&wrong).await.unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(sign_in_cookie(&res), "");
        let pg = std::env::var("DIRIGENT_CONTROL_POSTGRES").unwrap();
        let (db, conn) = tokio_postgres::connect(&pg, tokio_postgres::NoTls)
            .await
            .unwrap();
        tokio::spawn(conn);
        let id = Uuid::parse_str(id).unwrap();
        let expire = |at: &'static str| {
            let sql = format!("UPDATE dirigent.web_sessions SET expires = {at} WHERE id = $1");
            let db = &db;
            async move { db.execute(&sql, &[&id]).await.unwrap() }
        };
        expire("now() - interval '1 second'").await;
        assert_eq!(
            refresh(&cookie).await.unwrap().status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
        expire("now() + interval '1 hour'").await;
        assert_eq!(
            refresh(&cookie).await.unwrap().status(),
            reqwest::StatusCode::OK
        );

        // signed out, it's over
        let out = http
            .delete(format!("http://{base}/session"))
            .header(
                reqwest::header::COOKIE,
                format!("{}={cookie}", signin::COOKIE),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(out.status(), reqwest::StatusCode::NO_CONTENT);
        assert_eq!(sign_in_cookie(&out), "");
        assert_eq!(
            refresh(&cookie).await.unwrap().status(),
            reqwest::StatusCode::UNAUTHORIZED
        );
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
    /// the stack's chlive; its address. A real player takes a while to take its lease
    /// (connecting to kopia alone may take seconds), so it has longer.
    async fn start_local(root: &std::path::Path) -> Option<Ctl> {
        let root = root.to_path_buf();
        let (ttl, startup) = (Duration::from_secs(600), Duration::from_secs(15));
        start_all(
            None,
            ttl,
            false,
            None,
            None,
            no_github(),
            startup,
            None,
            None,
            |ctl| {
                Box::leak(Box::new(Local {
                    id: format!("local:{}", root.display()),
                    root: root.clone(),
                    program: concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/player").into(),
                    args: vec!["--mock".into()],
                    control_url: format!("http://{}", ctl.grpc),
                    control_http: format!("http://{}", ctl.http),
                    build: None,
                }))
            },
        )
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

    /// A configuration for the players' system of its own (so its hash is this test's), in a
    /// directory to load it from.
    fn configuration() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let module = format!("# {}\n{{ pkgs, ... }}: {{ }}\n", Uuid::new_v4());
        std::fs::write(dir.path().join("configuration.nix"), module).unwrap();
        dir
    }

    /// A control with the players' system configured in `dir`, its builds taking up to
    /// `timeout`.
    async fn start_system(
        runner: &'static Recorder,
        dir: &std::path::Path,
        timeout: Duration,
    ) -> Option<Ctl> {
        let system = System::load(dir, timeout).unwrap();
        let ttl = Duration::from_secs(600);
        start_all(
            None,
            ttl,
            false,
            None,
            None,
            no_github(),
            STARTUP,
            system,
            None,
            |_| runner,
        )
        .await
    }

    /// The builds `runner` was asked for, once there are `n`, within 10 s.
    async fn builds_n(runner: &Recorder, n: usize) -> Vec<(Uuid, String)> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let got = runner.builds.lock().unwrap().clone();
            if got.len() >= n || tokio::time::Instant::now() > deadline {
                return got;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// A build VM telling control how `build` went, with `token`.
    async fn report(
        base: &Ctl,
        build: Uuid,
        token: &str,
        body: serde_json::Value,
    ) -> reqwest::StatusCode {
        reqwest::Client::new()
            .post(format!("http://{base}/system/builds/{build}"))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status()
    }

    async fn system_state(base: &Ctl) -> serde_json::Value {
        get_json(base, A, "/system").await.1
    }

    /// The systems `session`'s players were started in so far.
    fn systems_of(runner: &Recorder, session: Uuid) -> Vec<Option<String>> {
        let systems = runner.systems.lock().unwrap();
        systems
            .iter()
            .filter(|(s, _)| *s == session)
            .map(|(_, system)| system.clone())
            .collect()
    }

    #[tokio::test]
    async fn e2e_the_players_system_is_built_once_for_its_configuration_and_version() {
        let dir = configuration();
        let runner = recorder(false);
        let hour = Duration::from_secs(3600);
        let Some(base) = start_system(runner, dir.path(), hour).await else {
            return;
        };
        let (build, token) = builds_n(runner, 1).await.pop().expect("a build");
        assert_eq!(system_state(&base).await["state"], "building");
        // meanwhile, sessions run the stock image
        let early = Uuid::new_v4();
        put(&base, early).await;
        post(&base, &format!("/sessions/{early}/start"), None).await;
        assert_eq!(systems_of(runner, early), vec![None]);

        let path = "/nix/store/0000000000000000000000000000000a-nixos-system-player";
        let done = serde_json::json!({ "ok": true, "store_path": path });
        assert_eq!(
            report(&base, build, "not its token", done.clone()).await,
            reqwest::StatusCode::NOT_FOUND
        );
        assert_eq!(
            report(&base, build, &token, done.clone()).await,
            reqwest::StatusCode::NO_CONTENT
        );
        // told once
        assert_eq!(
            report(&base, build, &token, done).await,
            reqwest::StatusCode::NOT_FOUND
        );
        assert_eq!(runner.ended.lock().unwrap().clone(), vec![build]);
        let state = system_state(&base).await;
        assert_eq!(
            (state["state"].as_str(), state["store_path"].as_str()),
            (Some("ready"), Some(path))
        );
        let s = Uuid::new_v4();
        put(&base, s).await;
        post(&base, &format!("/sessions/{s}/start"), None).await;
        assert_eq!(systems_of(runner, s), vec![Some(path.to_string())]);

        // a control started again, with the same configuration: built already
        let again = recorder(false);
        let Some(base) = start_system(again, dir.path(), hour).await else {
            return;
        };
        assert!(builds_n(again, 1).await.is_empty(), "built again");
        let t = Uuid::new_v4();
        put(&base, t).await;
        post(&base, &format!("/sessions/{t}/start"), None).await;
        assert_eq!(systems_of(again, t), vec![Some(path.to_string())]);

        // another configuration: built anew
        std::fs::write(dir.path().join("more.nix"), "{ }").unwrap();
        let other = recorder(false);
        start_system(other, dir.path(), hour).await;
        assert_eq!(builds_n(other, 1).await.len(), 1);
    }

    #[tokio::test]
    async fn e2e_a_failed_or_late_build_leaves_the_stock_image_until_control_tries_again() {
        let dir = configuration();
        let runner = recorder(false);
        let Some(base) = start_system(runner, dir.path(), Duration::from_secs(3600)).await else {
            return;
        };
        let (build, token) = builds_n(runner, 1).await.pop().expect("a build");
        let log = "error: undefined variable 'pkgs.nodjs'";
        assert_eq!(
            report(
                &base,
                build,
                &token,
                serde_json::json!({ "ok": false, "log": log })
            )
            .await,
            reqwest::StatusCode::NO_CONTENT
        );
        let state = system_state(&base).await;
        assert_eq!(
            (state["state"].as_str(), state["error"].as_str()),
            (Some("failed"), Some(log))
        );
        let s = Uuid::new_v4();
        put(&base, s).await;
        post(&base, &format!("/sessions/{s}/start"), None).await;
        assert_eq!(systems_of(runner, s), vec![None]);

        // started again, control tries again; this time the build takes too long
        let again = recorder(false);
        let Some(base) = start_system(again, dir.path(), Duration::from_secs(1)).await else {
            return;
        };
        let (late, _) = builds_n(again, 1).await.pop().expect("built again");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while system_state(&base).await["state"] != "failed" {
            assert!(tokio::time::Instant::now() < deadline, "not timed out");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(system_state(&base).await["error"]
            .as_str()
            .unwrap()
            .contains("not built in"));
        assert_eq!(again.ended.lock().unwrap().clone(), vec![late]);
    }

    #[tokio::test]
    async fn e2e_a_system_no_vm_comes_up_with_is_broken_and_one_that_does_isnt() {
        let dir = configuration();
        let runner = recorder(false);
        let hour = Duration::from_secs(3600);
        let Some(base) = start_system(runner, dir.path(), hour).await else {
            return;
        };
        let (build, token) = builds_n(runner, 1).await.pop().expect("a build");
        let path = "/nix/store/0000000000000000000000000000000b-nixos-system-player";
        let done = serde_json::json!({ "ok": true, "store_path": path });
        report(&base, build, &token, done).await;

        // its player comes, within its startup: the system works
        let came = Uuid::new_v4();
        put(&base, came).await;
        post(&base, &format!("/sessions/{came}/start"), None).await;
        let (player, epoch) = started_n(runner, came, 1).await.pop().expect("started");
        let mut conn = connect(&base).await;
        assert_eq!(
            acquire(&mut conn, came, &player, Some(epoch)).await,
            acquired(epoch)
        );
        tokio::time::sleep(STARTUP * 3).await;
        assert_eq!(system_state(&base).await["state"], "ready");

        // this one's doesn't, nor on the stock image: something else is wrong
        let neither = Uuid::new_v4();
        put(&base, neither).await;
        post(&base, &format!("/sessions/{neither}/start"), None).await;
        started_n(runner, neither, 3).await;
        let systems = systems_of(runner, neither);
        assert_eq!(
            systems[..3],
            [Some(path.to_string()), None, Some(path.to_string())]
        );
        assert_eq!(system_state(&base).await["state"], "ready");
        post(&base, &format!("/sessions/{neither}/archive"), None).await;

        // this one's doesn't, but does on the stock image: the system is why
        let lost = Uuid::new_v4();
        put(&base, lost).await;
        post(&base, &format!("/sessions/{lost}/start"), None).await;
        let (player, epoch) = started_n(runner, lost, 2)
            .await
            .pop()
            .expect("started again");
        assert_eq!(systems_of(runner, lost), vec![Some(path.to_string()), None]);
        let mut stock = connect(&base).await;
        assert_eq!(
            acquire(&mut stock, lost, &player, Some(epoch)).await,
            acquired(epoch)
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while system_state(&base).await["state"] != "broken" {
            assert!(tokio::time::Instant::now() < deadline, "not broken");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let state = system_state(&base).await;
        assert!(state["error"].as_str().unwrap().contains("didn't come up"));
        conn.close().await;
        stock.close().await;

        // broken, it isn't built again for the same configuration
        let again = recorder(false);
        let Some(base) = start_system(again, dir.path(), hour).await else {
            return;
        };
        assert!(builds_n(again, 1).await.is_empty(), "built again");
        let s = Uuid::new_v4();
        put(&base, s).await;
        post(&base, &format!("/sessions/{s}/start"), None).await;
        assert_eq!(systems_of(again, s), vec![None]);
    }

    #[tokio::test]
    async fn e2e_players_tell_what_their_vms_added_to_the_shared_store() {
        let runner = recorder(false);
        let Some(base) = start_recording(runner).await else {
            return;
        };
        let s = Uuid::new_v4();
        put(&base, s).await;
        post(&base, &format!("/sessions/{s}/start"), None).await;
        let (player, epoch) = started_n(runner, s, 1).await.pop().expect("started");
        let mut conn = connect(&base).await;
        assert_eq!(
            acquire(&mut conn, s, &player, Some(epoch)).await,
            acquired(epoch)
        );
        let token = conn.tokens.as_ref().expect("its tokens").player.clone();
        let id = Uuid::new_v4().simple();
        let (upstream, mine) = (
            format!("/nix/store/{id}-hello"),
            format!("/nix/store/{id}-mine"),
        );
        let body = serde_json::json!({ "paths": [
            { "path": upstream, "signed": true },
            { "path": mine, "signed": false },
            { "path": "/etc/passwd", "signed": true },
        ] });
        let tell = |token: Option<String>| {
            let req = reqwest::Client::new()
                .post(format!("http://{base}/nix/paths"))
                .json(&body);
            let req = match token {
                Some(t) => req.bearer_auth(t),
                None => req,
            };
            async move { req.send().await.unwrap().status() }
        };
        assert_eq!(tell(None).await, reqwest::StatusCode::UNAUTHORIZED);
        assert_eq!(
            tell(Some("not a token".into())).await,
            reqwest::StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            tell(Some(token.clone())).await,
            reqwest::StatusCode::NO_CONTENT
        );
        // told again (another VM added it too)
        assert_eq!(tell(Some(token)).await, reqwest::StatusCode::NO_CONTENT);

        let url = std::env::var("DIRIGENT_CONTROL_POSTGRES").unwrap();
        let store = Store::connect(&url, Duration::from_secs(1)).await.unwrap();
        let has = |paths: &[(String, bool)], p: &str| paths.iter().any(|(q, _)| q == p);
        let signed = store.unpromoted(false).await.unwrap();
        assert!(has(&signed, &upstream) && !has(&signed, &mine));
        let all = store.unpromoted(true).await.unwrap();
        assert!(has(&all, &upstream) && has(&all, &mine) && !has(&all, "/etc/passwd"));
        store
            .promoted(std::slice::from_ref(&upstream), 7)
            .await
            .unwrap();
        assert!(!has(&store.unpromoted(true).await.unwrap(), &upstream));
        conn.close().await;
    }

    #[tokio::test]
    async fn e2e_without_a_configuration_there_is_no_system_of_its_own() {
        let Some(base) = start_recording(recorder(false)).await else {
            return;
        };
        assert_eq!(
            system_state(&base).await,
            serde_json::json!({ "configured": false })
        );
    }

    #[tokio::test]
    async fn e2e_settings_are_each_users_own_defaulted_and_checked() {
        let Some(base) = start_recording(recorder(false)).await else {
            return;
        };
        let (user, other) = (new_user(), new_user());
        let (status, got) = get_json(&base, &user, "/settings").await;
        assert_eq!(status, reqwest::StatusCode::OK);
        assert_eq!(got, serde_json::json!({ "events": "activity" }));
        let put = |who: &str, body: serde_json::Value| {
            let (base, who) = (&base, who.to_string());
            async move {
                let put = reqwest::Method::PUT;
                send_json(base, &who, put, "/settings", body).await
            }
        };
        let (status, saved) = put(&user, serde_json::json!({ "events": "all" })).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{saved}");
        assert_eq!(get_json(&base, &user, "/settings").await.1["events"], "all");
        assert_eq!(
            get_json(&base, &other, "/settings").await.1["events"],
            "activity"
        );
        for bad in [
            serde_json::json!({ "events": "some" }),
            serde_json::json!({ "theme": "dark" }),
            serde_json::json!([1]),
        ] {
            let (status, why) = put(&user, bad).await;
            assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{why}");
        }
        assert_eq!(get_json(&base, &user, "/settings").await.1["events"], "all");
        // all of them each time: left out, back to its default
        put(&user, serde_json::json!({})).await;
        assert_eq!(
            get_json(&base, &user, "/settings").await.1["events"],
            "activity"
        );
    }

    #[tokio::test]
    async fn e2e_a_sessions_machine_is_asked_for_or_its_owners_default_within_the_limits() {
        let runner = recorder(false);
        let machines = Machines {
            limits: Partial {
                cpu: Some(8),
                ..Default::default()
            },
            per_user: true,
            ..test_machines()
        };
        let ttl = Duration::from_secs(600);
        let Some(base) = start_all(
            None,
            ttl,
            false,
            None,
            None,
            no_github(),
            STARTUP,
            None,
            Some(machines),
            |_| runner,
        )
        .await
        else {
            return;
        };
        let (user, other) = (new_user(), new_user());
        let (_, got) = get_json(&base, &user, "/machines").await;
        assert_eq!(
            got,
            serde_json::json!({
                "defaults": { "cpu": 2, "memory": 4, "disk": 20 },
                "limits": { "cpu": 8 },
                "per_user": true,
                "warm": 0,
            })
        );
        let put = reqwest::Method::PUT;
        let mine = serde_json::json!({ "machine": { "memory": 16 } });
        let (status, why) = send_json(&base, &user, put.clone(), "/settings", mine).await;
        assert_eq!(status, reqwest::StatusCode::OK, "{why}");
        let over = serde_json::json!({ "machine": { "cpu": 9 } });
        let (status, why) = send_json(&base, &user, put.clone(), "/settings", over).await;
        assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{why}");
        let machine = |cpu, memory, disk| Resources { cpu, memory, disk };
        let made = |who: String, body: serde_json::Value| {
            let base = &base;
            async move {
                let s = Uuid::new_v4();
                let status = put_json(base, s, &who, body).await;
                let (_, info) = get_json(base, &who, &format!("/sessions/{s}")).await;
                let resources = info["spec"].get("resources").cloned();
                (s, status, resources)
            }
        };
        let start = serde_json::json!({ "start": true });
        // the owner's default, the rest the cluster's
        let (s, status, kept) = made(user.clone(), start.clone()).await;
        assert_eq!(status, reqwest::StatusCode::CREATED);
        assert_eq!(
            kept,
            Some(serde_json::json!({ "cpu": 2, "memory": 16, "disk": 20 }))
        );
        let started = runner.machines.lock().unwrap().clone();
        assert!(started.contains(&(s, machine(2, 16, 20))), "{started:?}");
        // what's asked for, the rest the owner's
        let asked = serde_json::json!({ "start": true, "resources": { "cpu": 4 } });
        let (s, _, kept) = made(user.clone(), asked).await;
        assert_eq!(
            kept,
            Some(serde_json::json!({ "cpu": 4, "memory": 16, "disk": 20 }))
        );
        assert!(runner
            .machines
            .lock()
            .unwrap()
            .contains(&(s, machine(4, 16, 20))));
        // nothing asked for and no defaults of their own: the cluster's, whatever it is then
        let (s, _, kept) = made(other.clone(), start).await;
        assert_eq!(kept, None);
        assert!(runner
            .machines
            .lock()
            .unwrap()
            .contains(&(s, machine(2, 4, 20))));
        let over = serde_json::json!({ "resources": { "cpu": 9 } });
        let (_, status, _) = made(other.clone(), over).await;
        assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
        let none = serde_json::json!({ "resources": { "cpu": 0 } });
        let (_, status, _) = made(other, none).await;
        assert_eq!(status, reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn e2e_without_defaults_per_user_settings_have_no_machine() {
        let Some(base) = start_recording(recorder(false)).await else {
            return;
        };
        let user = new_user();
        let mine = serde_json::json!({ "machine": { "memory": 16 } });
        let put = reqwest::Method::PUT;
        let (status, why) = send_json(&base, &user, put, "/settings", mine).await;
        assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{why}");
        assert_eq!(
            get_json(&base, &user, "/machines").await.1["per_user"],
            false
        );
    }

    /// A control with `runner`, on `machines`.
    async fn start_machines(runner: &'static dyn Runner, machines: Machines) -> Option<Ctl> {
        let ttl = Duration::from_secs(600);
        start_all(
            None,
            ttl,
            false,
            None,
            None,
            no_github(),
            STARTUP,
            None,
            Some(machines),
            |_| runner,
        )
        .await
    }

    /// `player` waits at control as a warm player: the stream it's told its session on.
    async fn wait_for(base: &Ctl, player: &str) -> Result<Streaming<pb::Assigned>, Status> {
        let mut client = PlayersClient::connect(format!("http://{}", base.grpc))
            .await
            .unwrap();
        let ask = pb::WaitRequest {
            player: player.into(),
            system: None,
        };
        client.wait(ask).await.map(|r| r.into_inner())
    }

    /// `runner`'s warm VMs, once there are `n`, within 10 s.
    async fn warm_n(runner: &Recorder, n: usize) -> Vec<runner::Warm> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let warm = runner.warm.lock().unwrap().clone();
            if warm.len() >= n || tokio::time::Instant::now() > deadline {
                return warm;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn e2e_a_session_takes_a_warm_vm_on_its_machine_and_another_is_made() {
        let runner = warm_recorder();
        let machines = Machines {
            warm: 1,
            ..test_machines()
        };
        let Some(base) = start_machines(runner, machines).await else {
            return;
        };
        let [first] = &warm_n(runner, 1).await[..] else {
            panic!("no warm VM")
        };
        let mut told = wait_for(&base, &first.player).await.unwrap();
        let stranger = wait_for(&base, &Uuid::new_v4().to_string()).await;
        assert_eq!(stranger.unwrap_err().code(), Code::NotFound);

        let s = Uuid::new_v4();
        let start = serde_json::json!({ "start": true });
        assert_eq!(
            put_json(&base, s, A, start).await,
            reqwest::StatusCode::CREATED
        );
        let given = tokio::time::timeout(Duration::from_secs(10), told.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!((given.session, given.epoch), (s.to_string(), 1));
        assert_eq!(runner.of(s), vec![], "no VM of its own");
        let taken = runner.taken.lock().unwrap().clone();
        assert_eq!(taken, vec![(s, first.player.clone(), 1)]);
        let reserved = info(&base, s).await.player.unwrap();
        assert_eq!(reserved.subject, player_subject(&first.player));
        let mut conn = connect(&base).await;
        assert_eq!(
            acquire(&mut conn, s, &first.player, Some(1)).await,
            acquired(1)
        );
        // another, in its place
        let [next] = &warm_n(runner, 1).await[..] else {
            panic!("no other warm VM")
        };
        assert_ne!(next.player, first.player);

        // one on another machine waits for a VM of its own
        let s = Uuid::new_v4();
        let big = serde_json::json!({ "start": true, "resources": { "cpu": 4 } });
        put_json(&base, s, A, big).await;
        assert_eq!(runner.of(s).len(), 1);
        assert_eq!(runner.taken.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn e2e_without_a_warm_player_waiting_a_session_gets_a_vm_of_its_own() {
        let runner = warm_recorder();
        let machines = Machines {
            warm: 1,
            ..test_machines()
        };
        let Some(base) = start_machines(runner, machines).await else {
            return;
        };
        let [booting] = &warm_n(runner, 1).await[..] else {
            panic!("no warm VM")
        };
        // its player hasn't come: a VM of its own
        let s = Uuid::new_v4();
        put_json(&base, s, A, serde_json::json!({ "start": true })).await;
        assert_eq!(runner.of(s).len(), 1);
        // one whose VM isn't current any more doesn't wait
        runner.generation.fetch_add(1, Ordering::Relaxed);
        let stale = wait_for(&base, &booting.player).await;
        assert_eq!(stale.unwrap_err().code(), Code::NotFound);
    }

    #[tokio::test]
    async fn e2e_with_defaults_per_user_warm_vms_are_on_active_users_machines() {
        let runner = warm_recorder();
        let machines = Machines {
            warm: 1,
            per_user: true,
            ..test_machines()
        };
        let Some(base) = start_machines(runner, machines).await else {
            return;
        };
        let put = reqwest::Method::PUT;
        for cpu in [3, 5] {
            let user = new_user();
            let mine = serde_json::json!({ "machine": { "cpu": cpu, "memory": 1 } });
            send_json(&base, &user, put.clone(), "/settings", mine).await;
            put_json(&base, Uuid::new_v4(), &user, serde_json::json!({})).await;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let has = |cpu| {
            let warm = runner.warm.lock().unwrap();
            warm.iter().any(|w| {
                w.machine
                    == Resources {
                        cpu,
                        memory: 1,
                        disk: 20,
                    }
            })
        };
        while !(has(3) && has(5)) {
            assert!(tokio::time::Instant::now() < deadline, "{:?}", runner.warm);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Local players, and warm ones too, in `<root>/warm-<player>`.
    struct WarmLocal {
        local: Local,
        warm: std::sync::Mutex<Vec<runner::Warm>>,
    }

    #[async_trait::async_trait]
    impl Runner for WarmLocal {
        fn id(&self) -> &str {
            self.local.id()
        }

        async fn start(
            &self,
            session: Uuid,
            player: &str,
            epoch: i64,
            system: Option<&str>,
            machine: Resources,
        ) -> anyhow::Result<()> {
            self.local
                .start(session, player, epoch, system, machine)
                .await
        }

        async fn forget(&self, session: Uuid) -> anyhow::Result<()> {
            self.local.forget(session).await
        }

        fn warms(&self) -> bool {
            true
        }

        async fn warm(
            &self,
            player: &str,
            machine: Resources,
            _: Option<&str>,
            key: &str,
        ) -> anyhow::Result<()> {
            let dir = self.local.root.join(format!("warm-{player}"));
            std::fs::create_dir_all(&dir)?;
            let log = std::fs::File::create(dir.join("player.log"))?;
            let mut cmd = tokio::process::Command::new(&self.local.program);
            cmd.args(["--warm", "--player-id", player])
                .args(["--control-url", &self.local.control_url])
                .args(["--control-http", &self.local.control_http])
                .arg("--workdir")
                .arg(dir.join("workspace"))
                .arg("--state-dir")
                .arg(dir.join("state"))
                .args(&self.local.args)
                .env_clear()
                .envs(
                    runner::PASSED_ON
                        .iter()
                        .filter_map(|k| Some((k, std::env::var_os(k)?))),
                )
                .stdout(log.try_clone()?)
                .stderr(log);
            // gone with the test
            // SAFETY: prctl(2) alone, between fork and exec
            unsafe {
                cmd.pre_exec(|| {
                    libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                    Ok(())
                });
            }
            cmd.spawn()?;
            self.warm.lock().unwrap().push(runner::Warm {
                player: player.into(),
                machine,
                key: key.into(),
                age: Duration::ZERO,
            });
            Ok(())
        }

        async fn warm_vms(&self) -> anyhow::Result<Vec<runner::Warm>> {
            Ok(self.warm.lock().unwrap().clone())
        }

        async fn take(&self, _: Uuid, player: &str, _: i64) -> anyhow::Result<()> {
            self.warm.lock().unwrap().retain(|w| w.player != player);
            Ok(())
        }
    }

    #[tokio::test]
    async fn e2e_a_warm_local_player_takes_a_session_it_is_given() {
        let root = tempfile::tempdir().unwrap();
        let mut made: Option<&'static WarmLocal> = None;
        let machines = Machines {
            warm: 1,
            ..test_machines()
        };
        let ttl = Duration::from_secs(600);
        let Some(base) = start_all(
            None,
            ttl,
            false,
            None,
            None,
            no_github(),
            Duration::from_secs(15),
            None,
            Some(machines),
            |ctl| {
                let runner = Box::leak(Box::new(WarmLocal {
                    local: Local {
                        id: format!("local:{}", root.path().display()),
                        root: root.path().to_path_buf(),
                        program: concat!(env!("CARGO_MANIFEST_DIR"), "/../target/debug/player")
                            .into(),
                        args: vec!["--mock".into()],
                        control_url: format!("http://{}", ctl.grpc),
                        control_http: format!("http://{}", ctl.http),
                        build: None,
                    },
                    warm: Default::default(),
                }));
                made = Some(runner);
                runner
            },
        )
        .await
        else {
            return;
        };
        let runner = made.expect("made");
        // its player waiting
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let log = loop {
            let player = runner
                .warm
                .lock()
                .unwrap()
                .first()
                .map(|w| w.player.clone());
            let log = player.map(|p| root.path().join(format!("warm-{p}/player.log")));
            if let Some(log) = log.filter(|l| {
                std::fs::read_to_string(l).is_ok_and(|t| t.contains("waiting for a session"))
            }) {
                break log;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "no warm player waits"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        let s = Uuid::new_v4();
        put_json(&base, s, A, serde_json::json!({ "start": true })).await;
        let attached =
            |i: &SessionInfo| i.epoch == 1 && i.player.as_ref().is_some_and(|p| p.attached);
        info_until(&base, s, attached).await;
        assert!(
            !root.path().join(s.to_string()).exists(),
            "no player of its own"
        );
        let stop = format!("/sessions/{s}/stop");
        post(&base, &stop, None).await;
        info_until(&base, s, |i| i.player.is_none()).await;
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains(&s.to_string()), "{text}");
    }

    /// A control that keeps secrets, with the fake GitHub's App.
    async fn start_github() -> Option<(Ctl, github::fake::GitHub, tempfile::TempDir)> {
        std::env::var_os("DIRIGENT_E2E")?;
        let gh = github::fake::GitHub::start().await;
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("link.key");
        seal::keygen(&key).unwrap();
        let args = KeyArgs {
            anthropic_api: "http://127.0.0.1:1".into(),
        };
        let keys = Some((args, Seal::load(&key).unwrap()));
        let runner = recorder(false);
        let ttl = Duration::from_secs(600);
        let github = gh.args(dir.path());
        let ctl = start_all(
            None,
            ttl,
            false,
            None,
            keys,
            github,
            STARTUP,
            None,
            None,
            |_| runner,
        )
        .await?;
        Some((ctl, gh, dir))
    }

    /// `user` starts connecting GitHub: the `state` GitHub is sent with, and the cookie.
    async fn start_connecting(base: &Ctl, user: &str) -> (String, String) {
        let res = call(user, reqwest::Method::POST, base, "/links/github/start")
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::OK);
        let cookie = res.headers()["set-cookie"].to_str().unwrap();
        assert!(
            cookie.contains("HttpOnly; Secure; SameSite=Lax"),
            "{cookie}"
        );
        let cookie = cookie.split(';').next().unwrap().to_string();
        let body: serde_json::Value = res.json().await.unwrap();
        let url = reqwest::Url::parse(body["url"].as_str().unwrap()).unwrap();
        assert!(url
            .path()
            .ends_with("/apps/dirigent-test/installations/new"));
        let state = url.query_pairs().find(|(k, _)| k == "state").unwrap().1;
        (state.into_owned(), cookie)
    }

    /// GitHub sends a browser with `cookie` back with `code` and `state`: where it's sent on.
    async fn come_back(base: &Ctl, cookie: Option<&str>, code: &str, state: &str) -> String {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let mut url = reqwest::Url::parse(&format!("http://{base}/links/github/callback")).unwrap();
        url.query_pairs_mut()
            .append_pair("code", code)
            .append_pair("state", state)
            .append_pair("setup_action", "install");
        let mut req = client.get(url);
        if let Some(cookie) = cookie {
            req = req.header("cookie", cookie);
        }
        let res = req.send().await.unwrap();
        assert_eq!(res.status(), reqwest::StatusCode::SEE_OTHER);
        res.headers()["location"].to_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn e2e_github_access_is_public_a_token_or_the_app_whose_tokens_sessions_get() {
        let Some((base, gh, _dir)) = start_github().await else {
            return;
        };
        let (user, other) = (new_user(), new_user());
        let login = format!("gh{}", &Uuid::new_v4().simple().to_string()[..8]);
        let access = |repo: String| {
            let (base, user) = (&base, user.clone());
            async move {
                let url = format!("https://github.com/{repo}");
                let path = format!("/github/access?url={}", url.replace('/', "%2F"));
                get_json(base, &user, &path).await.1
            }
        };
        let (private, other_repo) = (format!("{login}/private"), format!("{login}/other"));

        // nothing connected: public ones only
        assert_eq!(access(format!("{login}/public")).await["via"], "public");
        let got = access(private.clone()).await;
        assert_eq!(
            (&got["via"], &got["app"]),
            (&serde_json::Value::Null, &true.into())
        );
        assert_eq!(
            get_json(
                &base,
                &user,
                "/github/access?url=https%3A%2F%2Fgitlab.com%2Fa%2Fb"
            )
            .await
            .1["repo"],
            serde_json::Value::Null
        );

        // the user's GITHUB_TOKEN: refused, then taken
        let token = |value: &str| {
            let (base, user) = (&base, user.clone());
            let body = serde_json::json!({ "value": value });
            async move {
                let put = reqwest::Method::PUT;
                send_json(base, &user, put, "/secrets/GITHUB_TOKEN", body)
                    .await
                    .0
            }
        };
        assert_eq!(token("ghp_bad_one").await, reqwest::StatusCode::NO_CONTENT);
        let got = access(private.clone()).await;
        assert!(
            got["why"]
                .as_str()
                .unwrap()
                .contains("refuses your GITHUB_TOKEN"),
            "{got}"
        );
        token("ghp_good").await;
        assert_eq!(access(private.clone()).await["via"], "token");
        let gone = delete(&base, &user, "/secrets/GITHUB_TOKEN").await;
        assert_eq!(gone, reqwest::StatusCode::NO_CONTENT);

        // connecting: only the browser that started it, with a state of its own, links
        let (state, cookie) = start_connecting(&base, &user).await;
        let (others_state, others_cookie) = start_connecting(&base, &other).await;
        let code = format!("code-{login}");
        for (cookie, state) in [
            (None, state.as_str()),
            (Some(others_cookie.as_str()), state.as_str()),
            (Some(cookie.as_str()), others_state.as_str()),
            (Some(cookie.as_str()), "not-a-state"),
        ] {
            let to = come_back(&base, cookie, &code, state).await;
            assert!(to.starts_with("/?github_error="), "{to}");
        }
        let to = come_back(&base, Some(&cookie), "wrong", &state).await;
        assert!(to.contains("incorrect"), "{to}");
        let (_, linked) = get_json(&base, &user, "/links/github").await;
        assert_eq!(linked, serde_json::json!({ "app": true, "accounts": [] }));
        assert_eq!(
            come_back(&base, Some(&cookie), &code, &state).await,
            "/?github=connected"
        );
        let (_, linked) = get_json(&base, &user, "/links/github").await;
        assert_eq!(
            linked,
            serde_json::json!({ "app": true, "accounts": [login] })
        );
        let (_, theirs) = get_json(&base, &other, "/links/github").await;
        assert_eq!(theirs["accounts"], serde_json::json!([]));

        // the App reaches the repositories its installation includes
        assert_eq!(access(private.clone()).await["via"], "app");
        let got = access(other_repo.clone()).await;
        assert!(
            got["why"].as_str().unwrap().contains("doesn't include"),
            "{got}"
        );

        // a session on one gets a token of the App for it, minted once while it lasts; one
        // with a GITHUB_TOKEN of its own keeps that
        let minted = gh.minted.load(Ordering::SeqCst);
        let on = |repo: &str| serde_json::json!({ "source": { "git": { "url": format!("https://github.com/{repo}.git") } } });
        let (s, t, u) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        put_json(&base, s, &user, on(&private)).await;
        put_json(&base, t, &user, on(&private)).await;
        put_json(&base, u, &user, on(&private)).await;
        let own = serde_json::json!({ "value": "ghp_sessions_own" });
        let put = reqwest::Method::PUT;
        send_json(
            &base,
            &user,
            put,
            &format!("/sessions/{u}/secrets/GITHUB_TOKEN"),
            own,
        )
        .await;
        let mut held = Vec::new();
        for session in [s, t, u] {
            let mut conn = connect(&base).await;
            acquire(&mut conn, session, &Uuid::new_v4().to_string(), None).await;
            held.push(conn);
        }
        let token_of = |conn: &Conn| {
            conn.tokens
                .clone()
                .unwrap()
                .secrets
                .get("GITHUB_TOKEN")
                .cloned()
        };
        let app = token_of(&held[0]).unwrap();
        assert!(app.starts_with("ghs_installation7_"), "{app}");
        assert_eq!(token_of(&held[1]), Some(app));
        assert_eq!(token_of(&held[2]).as_deref(), Some("ghp_sessions_own"));
        // the one minted for the access check above, still good
        assert_eq!(gh.minted.load(Ordering::SeqCst), minted);

        // disconnected: no more
        assert_eq!(
            delete(&base, &user, "/links/github").await,
            reqwest::StatusCode::NO_CONTENT
        );
        let got = next_tokens(&mut held[0]).await;
        assert_eq!(got.secrets.get("GITHUB_TOKEN"), None);
        assert_eq!(access(private).await["via"], serde_json::Value::Null);
        for conn in held {
            conn.close().await;
        }
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
