//! The player: a headless coding agent holding one session of dirigent's, from control's lease,
//! its log in chlive, and its workspace here. [`run`] runs one, in a process of its own.

mod agent;
mod checkpoints;
mod compact;
mod dirigent;
mod history;
mod hook;
#[cfg(test)]
mod logs;
mod mirror;
mod nixstore;
mod process;
mod provider;
mod secrets;
mod session;
mod stop;
mod title;
mod tools;
mod workspace;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use anyhow::Context;
use clap::builder::styling::{AnsiColor, Styles};
use clap::Parser;
use tracing::{info, warn};

use crate::agent::{Agent, Detach, Model, Models};
use crate::checkpoints::Checkpoints;
use crate::dirigent::Director;
use crate::mirror::Mirror;
use crate::provider::{Effort, Provider};
use crate::secrets::Secrets;
use crate::session::chlive::ChliveSession;
use crate::session::control::Lease;
use crate::session::Session;
use crate::tools::Registry;
use dirigent_protocol::control::{Part, Provider as ModelProvider, SessionSpec, Settings};
use dirigent_protocol::Event;

pub use crate::session::control::channel as control_channel;

/// This player's version, as the build tells it (nix: `<the chart's appVersion>+<commit>`);
/// `dev` from cargo.
pub const VERSION: &str = match option_env!("DIRIGENT_VERSION") {
    Some(v) => v,
    None => "dev",
};

const SYSTEM_PROMPT: &str = include_str!("../prompts/system.md");
/// Where the cluster's players run, as the system prompt says.
const ENVIRONMENT: &str = include_str!("../prompts/environment.md");
/// The dirigent's, whole: its workspace isn't its work.
const DIRIGENT_PROMPT: &str = include_str!("../prompts/dirigent.md");

const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::BrightMagenta.on_default().bold())
    .usage(AnsiColor::BrightMagenta.on_default().bold())
    .literal(AnsiColor::BrightCyan.on_default())
    .placeholder(AnsiColor::BrightYellow.on_default())
    .valid(AnsiColor::BrightGreen.on_default())
    .invalid(AnsiColor::BrightRed.on_default())
    .error(AnsiColor::BrightRed.on_default().bold());

#[derive(Parser, Debug)]
#[command(
    name = "player",
    about = "headless coding agent, a worker of dirigent",
    styles = HELP_STYLES
)]
pub struct Args {
    /// The session to run (a UUID), made through control; if another player holds it, this one
    /// stops.
    #[arg(long, env = "DIRIGENT_SESSION", required_unless_present = "warm")]
    session: Option<String>,

    /// Warm: started before any session asked for one (its machine one that control keeps
    /// ready), it gets ready for one, and waits for control to give it one.
    #[arg(long, env = "DIRIGENT_WARM", conflicts_with_all = ["session", "epoch"], requires = "player_id")]
    warm: bool,

    /// Who this player is (a UUID; a new one when absent): to control, which leases it the
    /// session, and to chlive, as `player:<id>`.
    #[arg(long, env = "DIRIGENT_PLAYER_ID")]
    player_id: Option<String>,

    /// The lease control reserved for this player (it started it): resumed, not taken anew.
    #[arg(long, env = "DIRIGENT_EPOCH", hide = true)]
    epoch: Option<i64>,

    /// That lease's token, for this player alone to take it.
    #[arg(
        long,
        env = "DIRIGENT_LEASE_TOKEN",
        hide_env_values = true,
        hide = true
    )]
    lease_token: Option<String>,

    /// Control's gRPC service for players, where the session is leased.
    #[arg(
        long,
        env = "DIRIGENT_CONTROL_URL",
        default_value = "http://127.0.0.1:8091"
    )]
    control_url: String,

    /// Control's HTTP API, which a dirigent directs sessions through.
    #[arg(
        long,
        env = "DIRIGENT_CONTROL_HTTP",
        default_value = "http://127.0.0.1:8090"
    )]
    control_http: String,

    /// chlive's WebSocket, where the session's log is read and appended to.
    #[arg(long, env = "CHLIVE_URL", default_value = "ws://127.0.0.1:8080/live")]
    chlive_url: String,

    #[arg(long, env = "MOCK")]
    mock: bool,

    /// The session's workspace, made if it's missing; the spec's source is cloned into it when
    /// it's empty. The model and effort come from the spec too.
    #[arg(long, env = "WORKDIR", default_value = ".")]
    workdir: PathBuf,

    /// The kopia repository the workspace is checkpointed to after every turn, and restored
    /// from when it's found gone (`kopia repository status -t`). Without it, no checkpoints.
    #[arg(long, env = "DIRIGENT_KOPIA_TOKEN", hide_env_values = true)]
    kopia_token: Option<String>,

    /// The player's own state, outside the workspace: kopia's config and cache, and the
    /// repository the workspace is mirrored in. A new directory in the system's temporary one
    /// when absent.
    #[arg(long, env = "DIRIGENT_STATE_DIR")]
    state_dir: Option<PathBuf>,

    /// The model's context window in tokens; the context is compacted at 85% of it. Without
    /// it, the provider's: Claude's 200k, or the 272k GPT models take in.
    #[arg(long, env = "DIRIGENT_CONTEXT_WINDOW")]
    context_window: Option<u64>,

    /// The model that names the session (after its first message and each compaction).
    /// Without it, the provider's: Claude's Haiku, or, on a ChatGPT plan (whose models may
    /// have no small one), the session's model thinking little.
    #[arg(long, env = "DIRIGENT_TITLE_MODEL")]
    title_model: Option<String>,

    /// Stopped from outside (SIGTERM: its machine is going), how long the turn running may
    /// take to finish before it's cut, in seconds; then the workspace is checkpointed and the
    /// session handed over to the next player. Less than the machine's own grace period.
    #[arg(long, env = "DIRIGENT_STOP_GRACE", default_value_t = 300)]
    stop_grace: u64,

    /// What the system prompt says of where the player runs (a file, Markdown bullets): by
    /// default, the cluster's VMs (NixOS, as root).
    #[arg(long, env = "DIRIGENT_ENVIRONMENT")]
    environment: Option<PathBuf>,

    /// Where the player runs, by name, when it's not the cluster's VM (a host's): in its
    /// session's log, so the conversation notes the session moving.
    #[arg(long, env = "DIRIGENT_ENVIRONMENT_NAME")]
    environment_name: Option<String>,

    /// Anthropic's API (its key comes on the lease: the owner's, or the install's).
    #[arg(long, env = "ANTHROPIC_BASE_URL")]
    base_url: Option<String>,

    /// OpenAI's API, which a ChatGPT-plan session's model calls go to (keep the `/v1/`).
    #[arg(
        long,
        env = "OPENAI_BASE_URL",
        default_value = "https://api.openai.com/v1/"
    )]
    openai_base_url: String,

    /// Hand the session to the system while it runs, in this directory: its `env`, its secrets, the
    /// VM's hostname and `dirigent-session.target` (see `hook.rs`). The players' VM sets it.
    #[arg(long, env = "DIRIGENT_SESSION_HOOK")]
    session_hook: Option<PathBuf>,
}

/// Run the player `args` describe, until its session is done with it: in a process of its own,
/// whose logging, signals and memory it takes for itself (the `player` binary's, or `dirigent
/// host`'s for each session).
pub async fn run(args: Args) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dirigent_player=info,genai=warn".into()),
        )
        // colored only on a terminal: control writes it to a file
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();
    let cli: &'static Args = Box::leak(Box::new(args));
    let mut signals = stop::signals();

    std::fs::create_dir_all(&cli.workdir)
        .with_context(|| format!("--workdir {}", cli.workdir.display()))?;
    let workdir: &'static Path = Box::leak(
        cli.workdir
            .canonicalize()
            .with_context(|| format!("--workdir {}", cli.workdir.display()))?
            .into_boxed_path(),
    );
    let environment: &'static str = match &cli.environment {
        Some(path) => Box::leak(
            std::fs::read_to_string(path)
                .with_context(|| format!("--environment {}", path.display()))?
                .into_boxed_str(),
        ),
        None => ENVIRONMENT,
    };

    let player = match &cli.player_id {
        Some(p) => uuid(p, "--player-id")?,
        None => uuid::Uuid::new_v4().to_string(),
    };
    let state = match &cli.state_dir {
        Some(dir) => dir.clone(),
        None => std::env::temp_dir().join(format!("dirigent-player-{player}")),
    };
    std::fs::create_dir_all(&state)?;
    anyhow::ensure!(
        !state.canonicalize()?.starts_with(workdir),
        "--state-dir {} is inside the workspace",
        state.display()
    );
    let session = match &cli.session {
        Some(s) => Some(uuid(s, "--session")?),
        None => None,
    };
    let connected = match &cli.kopia_token {
        Some(token) => Some(Checkpoints::connect(token, &state, session.as_deref()).await?),
        None => {
            warn!("no --kopia-token: the workspace isn't checkpointed");
            None
        }
    };
    // warm, ready as it can be without a session, then given one
    let (id, epoch, lease_token) = match session {
        Some(id) => (id, cli.epoch, cli.lease_token.clone()),
        None => {
            let system = std::fs::read_link("/run/current-system")
                .ok()
                .map(|p| p.display().to_string());
            info!(player, system, "warm: ready for a session");
            let wait = session::control::wait(&cli.control_url, &player, system.as_deref());
            let (s, epoch, token) = tokio::select! {
                given = wait => given?,
                _ = signals.recv() => {
                    info!("stopped while waiting for a session");
                    return Ok(());
                }
            };
            (
                uuid(&s, "the session control gave")?,
                Some(epoch),
                Some(token),
            )
        }
    };
    let checkpoints: Option<&'static Checkpoints> = match connected {
        Some(c) if cli.warm => Some(Box::leak(Box::new(c.for_session(&id).await?))),
        Some(c) => Some(Box::leak(Box::new(c))),
        None => None,
    };
    info!(
        session = id,
        player,
        control = cli.control_url,
        "acquiring the session"
    );
    // Stopped meanwhile, the lease control may be granting is still taken (for a while), to be
    // handed over at once: left, the session would wait out its grace period.
    let acquire = Lease::acquire(&cli.control_url, &id, &player, epoch, lease_token);
    tokio::pin!(acquire);
    let (lease, stopped) = tokio::select! {
        lease = &mut acquire => (lease?, false),
        _ = signals.recv() => {
            info!("stopped while acquiring the session");
            match tokio::time::timeout(Duration::from_secs(30), &mut acquire).await {
                Ok(lease) => (lease?, true),
                Err(_) => return Ok(()),
            }
        }
    };
    let stop = stop::watch(
        lease.detach(),
        signals,
        Duration::from_secs(cli.stop_grace),
        stopped,
    );
    let spec: &'static SessionSpec = Box::leak(Box::new(lease.spec.clone()));
    let mut session = ChliveSession::new(&id, lease.epoch, &cli.chlive_url, lease.tokens())
        .in_environment(cli.environment_name.clone())
        .with_settings(lease.settings().borrow().clone());
    let hook = match &cli.session_hook {
        Some(dir) => {
            Some(hook::Hook::start(dir, &id, &lease.spec, Secrets::new(lease.tokens())).await)
        }
        None => None,
    };
    // on the shared Nix store, what this VM adds to it is told to control, now and then and at the end
    let tokens = lease.tokens();
    let reporter =
        nixstore::Reporter::new(&cli.control_http, move || tokens.borrow().player.clone())
            .map(|r| &*Box::leak(Box::new(Mutex::new(r))));
    if let Some(reporter) = reporter {
        tokio::spawn(async move {
            let mut every = tokio::time::interval(Duration::from_secs(120));
            loop {
                every.tick().await;
                reporter.lock().await.report().await;
            }
        });
    }
    let run = async {
        // the spec's, which the lease came with, then as control switches them
        let models = Models {
            settings: lease.settings(),
            make: model_maker(cli, &lease, id.to_string()),
        };
        let (log, mirror) = workspace::open(
            &mut session,
            workdir,
            spec.source.as_ref(),
            checkpoints,
            &state.join("sync"),
            Secrets::new(lease.tokens()),
        )
        .await?;
        let mirror: Option<&'static Mutex<Mirror>> =
            mirror.map(|m| &*Box::leak(Box::new(Mutex::new(m))));
        let (system, tools): (&'static str, Registry) = match spec.part {
            // after the workspace is made: its AGENTS.md is part of the prompt
            Part::Coder => (
                Box::leak(build_system_prompt(workdir, environment).into_boxed_str()),
                Registry::default_set(),
            ),
            Part::Dirigent => {
                let director = Arc::new(Director::new(
                    &id,
                    lease.tokens(),
                    &cli.chlive_url,
                    &cli.control_http,
                )?);
                tokio::spawn(dirigent::follow(director.clone(), cli.chlive_url.clone()));
                (DIRIGENT_PROMPT, Registry::new(director.tools()))
            }
        };
        let agent = Agent {
            system,
            models,
            tools: Arc::new(tools),
            workdir,
            checkpoints,
            mirror,
            secrets: Secrets::new(lease.tokens()),
        };
        info!(
            session = id,
            player,
            epoch = lease.epoch,
            chlive = cli.chlive_url,
            provider = ?spec.provider,
            model = spec.model,
            effort = ?spec.effort,
            part = ?spec.part,
            workdir = %workdir.display(),
            "running the session"
        );
        let detach = Detach {
            epoch: lease.epoch,
            when: stop.when.clone(),
        };
        agent.run(&mut session, log, detach).await
    };
    let result = tokio::select! {
        r = run => Some(r),
        _ = lease.lost() => None,
    };
    if let Some(hook) = hook {
        hook.end().await;
    }
    let Some(result) = result else {
        anyhow::bail!("the session's lease is lost to another player");
    };
    // failed: the session is given back, saying why
    if let Err(e) = &result {
        let failed = Event::PlayerFailed {
            epoch: lease.epoch,
            error: format!("{e:#}"),
        };
        let _ = tokio::time::timeout(Duration::from_secs(5), session.publish(&failed)).await;
    }
    if let Some(reporter) = reporter {
        let _ = tokio::time::timeout(Duration::from_secs(60), async {
            reporter.lock().await.report().await
        })
        .await;
    }
    if result.is_ok() && *stop.outside.borrow() {
        info!("handing the session over");
        lease.handoff().await;
    } else {
        lease.release().await;
    }
    result
}

/// What makes the session's [`Model`] of its settings, with the lease's credentials for either
/// provider (control sends the one the settings pay with). With `--mock`, the same mock always.
#[allow(clippy::type_complexity)]
fn model_maker(
    cli: &'static Args,
    lease: &Lease,
    session: String,
) -> Box<dyn Fn(&Settings) -> anyhow::Result<Model> + Send + Sync> {
    let (key, plan) = (lease.anthropic_key(), lease.plan_tokens());
    let mock: Option<Arc<dyn Provider>> = cli
        .mock
        .then(|| Arc::new(provider::mock::MockProvider::long()) as Arc<dyn Provider>);
    Box::new(move |settings: &Settings| {
        let provider: Arc<dyn Provider> = match (&mock, settings.provider) {
            (Some(mock), _) => mock.clone(),
            (None, ModelProvider::Anthropic) => Arc::new(
                provider::genai::GenaiProvider::anthropic(key.clone(), cli.base_url.clone())?,
            ),
            (None, ModelProvider::Chatgpt) => Arc::new(provider::genai::GenaiProvider::chatgpt(
                plan.clone(),
                cli.openai_base_url.clone(),
                session.clone(),
            )?),
        };
        let context_window = cli.context_window.unwrap_or(match settings.provider {
            ModelProvider::Anthropic => 200_000,
            ModelProvider::Chatgpt => 272_000,
        });
        let (title_model, title_effort) = match settings.provider {
            ModelProvider::Anthropic => ("claude-haiku-5-5".to_string(), None),
            ModelProvider::Chatgpt => (settings.model.clone(), Some(Effort::Low)),
        };
        Ok(Model {
            settings: settings.clone(),
            provider,
            context_window,
            title_model: cli.title_model.clone().unwrap_or(title_model),
            title_effort,
        })
    })
}

fn uuid(s: &str, what: &str) -> anyhow::Result<String> {
    Ok(uuid::Uuid::parse_str(s)
        .with_context(|| format!("{what} {s}: not a UUID"))?
        .to_string())
}

fn build_system_prompt(workdir: &Path, environment: &str) -> String {
    let mut prompt = SYSTEM_PROMPT
        .replace("{environment}", environment.trim())
        .trim_end()
        .to_string();
    let mut ctx = String::new();
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let path = workdir.join(name);
        if let Ok(text) = std::fs::read_to_string(&path) {
            if !text.trim().is_empty() {
                ctx.push_str(&format!(
                    "\n<project_instructions path=\"{}\">\n{}\n</project_instructions>",
                    path.display(),
                    text.trim_end()
                ));
            }
        }
    }
    if !ctx.is_empty() {
        prompt.push_str("\n\n<project_context>");
        prompt.push_str(&ctx);
        prompt.push_str("\n</project_context>");
    }
    prompt.push_str(&format!("\n\nWorking directory: {}\n", workdir.display()));
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_system_prompt_says_where_the_player_runs() {
        let dir = tempfile::tempdir().unwrap();
        let vm = build_system_prompt(dir.path(), ENVIRONMENT);
        assert!(vm.contains("# Environment\n\n- You run as root on NixOS"));
        let mac = build_system_prompt(dir.path(), "- You run as adam on macOS 15.\n");
        assert!(mac.contains("# Environment\n\n- You run as adam on macOS 15.\n\n# Reporting"));
        assert!(!mac.contains("NixOS") && !mac.contains("{environment}"));
    }
}
