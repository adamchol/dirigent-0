mod agent;
mod checkpoints;
mod compact;
mod dirigent;
mod history;
mod process;
mod provider;
mod session;
mod stop;
mod tools;
mod workspace;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::builder::styling::{AnsiColor, Styles};
use clap::Parser;
use tracing::{info, warn};

use crate::agent::{Agent, AgentConfig, Detach};
use crate::checkpoints::Checkpoints;
use crate::dirigent::Director;
use crate::provider::Provider;
use crate::session::chlive::ChliveSession;
use crate::session::control::Lease;
use crate::session::Session;
use crate::tools::Registry;
use dirigent_protocol::control::{Part, SessionSpec};
use dirigent_protocol::Event;

const SYSTEM_PROMPT: &str = include_str!("../prompts/system.md");
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
struct Cli {
    /// The session to run (a UUID), made through control; if another player holds it, this one
    /// stops.
    #[arg(long, env = "DIRIGENT_SESSION")]
    session: String,

    /// Who this player is (a UUID; a new one when absent): to control, which leases it the
    /// session, and to chlive, as `player:<id>`.
    #[arg(long, env = "DIRIGENT_PLAYER_ID")]
    player_id: Option<String>,

    /// The lease control reserved for this player (it started it): resumed, not taken anew.
    #[arg(long, env = "DIRIGENT_EPOCH", hide = true)]
    epoch: Option<i64>,

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

    /// The player's own state, outside the workspace: kopia's config and cache. A new
    /// directory in the system's temporary one when absent.
    #[arg(long, env = "DIRIGENT_STATE_DIR")]
    state_dir: Option<PathBuf>,

    /// The model's context window in tokens; the context is compacted at 85% of it.
    #[arg(long, env = "DIRIGENT_CONTEXT_WINDOW", default_value_t = 200_000)]
    context_window: u64,

    /// Stopped from outside (SIGTERM: its machine is going), how long the turn running may
    /// take to finish before it's cut, in seconds; then the workspace is checkpointed and the
    /// session handed over to the next player. Less than the machine's own grace period.
    #[arg(long, env = "DIRIGENT_STOP_GRACE", default_value_t = 300)]
    stop_grace: u64,

    #[arg(long, env = "ANTHROPIC_API_KEY", hide_env_values = true)]
    api_key: Option<String>,

    #[arg(long, env = "ANTHROPIC_BASE_URL")]
    base_url: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "player=info,genai=warn".into()),
        )
        // colored only on a terminal: control writes it to a file
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
        .init();
    let cli: &'static Cli = Box::leak(Box::new(Cli::parse()));
    let mut signals = stop::signals();

    let provider: Arc<dyn Provider> = if cli.mock {
        Arc::new(provider::mock::MockProvider::long())
    } else {
        let key = cli
            .api_key
            .clone()
            .context("--api-key / ANTHROPIC_API_KEY is required unless --mock")?;
        Arc::new(provider::genai::GenaiProvider::new(
            key,
            cli.base_url.clone(),
        )?)
    };

    std::fs::create_dir_all(&cli.workdir)
        .with_context(|| format!("--workdir {}", cli.workdir.display()))?;
    let workdir: &'static Path = Box::leak(
        cli.workdir
            .canonicalize()
            .with_context(|| format!("--workdir {}", cli.workdir.display()))?
            .into_boxed_path(),
    );

    let id = uuid(&cli.session, "--session")?;
    let player = match &cli.player_id {
        Some(p) => uuid(p, "--player-id")?,
        None => uuid::Uuid::new_v4().to_string(),
    };
    let checkpoints: Option<&'static Checkpoints> = match &cli.kopia_token {
        Some(token) => {
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
            let checkpoints = Checkpoints::connect(token, &state, &id).await?;
            Some(Box::leak(Box::new(checkpoints)))
        }
        None => {
            warn!("no --kopia-token: the workspace isn't checkpointed");
            None
        }
    };
    info!(
        session = id,
        player,
        control = cli.control_url,
        "acquiring the session"
    );
    // Stopped meanwhile, the lease control may be granting is still taken (for a while), to be
    // handed over at once: left, the session would wait out its grace period.
    let acquire = Lease::acquire(&cli.control_url, &id, &player, cli.epoch);
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
    let mut session = ChliveSession::new(&id, lease.epoch, &cli.chlive_url, lease.tokens());
    let run = async {
        let log = workspace::open(&mut session, workdir, spec.source.as_ref(), checkpoints).await?;
        let (system, tools): (&'static str, Registry) = match spec.part {
            // after the workspace is made: its AGENTS.md is part of the prompt
            Part::Coder => (
                Box::leak(build_system_prompt(workdir).into_boxed_str()),
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
            provider: provider.clone(),
            cfg: AgentConfig {
                model: &spec.model,
                system,
                effort: spec.effort,
                context_window: cli.context_window,
            },
            tools: Arc::new(tools),
            workdir,
            checkpoints,
        };
        info!(
            session = id,
            player,
            epoch = lease.epoch,
            chlive = cli.chlive_url,
            provider = provider.name(),
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
        r = run => r,
        _ = lease.lost() => anyhow::bail!("the session's lease is lost to another player"),
    };
    // failed: the session is given back, saying why
    if let Err(e) = &result {
        let failed = Event::PlayerFailed {
            epoch: lease.epoch,
            error: format!("{e:#}"),
        };
        let _ = tokio::time::timeout(Duration::from_secs(5), session.publish(&failed)).await;
    }
    if result.is_ok() && *stop.outside.borrow() {
        info!("handing the session over");
        lease.handoff().await;
    } else {
        lease.release().await;
    }
    result
}

fn uuid(s: &str, what: &str) -> anyhow::Result<String> {
    Ok(uuid::Uuid::parse_str(s)
        .with_context(|| format!("{what} {s}: not a UUID"))?
        .to_string())
}

fn build_system_prompt(workdir: &Path) -> String {
    let mut prompt = SYSTEM_PROMPT.trim_end().to_string();
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
