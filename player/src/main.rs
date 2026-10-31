mod agent;
mod compact;
mod history;
mod provider;
mod session;
mod tools;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use clap::builder::styling::{AnsiColor, Styles};
use clap::Parser;
use tracing::info;

use crate::agent::{Agent, AgentConfig};
use crate::provider::{Effort, Provider};
use crate::session::chlive::ChliveSession;
use crate::tools::Registry;

const SYSTEM_PROMPT: &str = include_str!("../prompts/system.md");

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
    /// The session to run (a UUID); a new one when absent.
    #[arg(long, env = "DIRIGENT_SESSION")]
    session: Option<String>,

    /// chlive's WebSocket, where the session's log is read.
    #[arg(long, env = "CHLIVE_URL", default_value = "ws://127.0.0.1:8080/live")]
    chlive_url: String,

    /// Kafka bootstrap servers, where the session's log is appended to.
    #[arg(long, env = "DIRIGENT_KAFKA", default_value = "127.0.0.1:9092")]
    kafka: String,

    #[arg(long, env = "MOCK")]
    mock: bool,

    #[arg(long, env = "WORKDIR", default_value = ".")]
    workdir: PathBuf,

    #[arg(long, env = "DIRIGENT_MODEL", default_value = "claude-sonnet-5")]
    model: String,

    #[arg(long, value_enum, default_value = "medium")]
    effort: Option<Effort>,

    /// The model's context window in tokens; the context is compacted at 85% of it.
    #[arg(long, env = "DIRIGENT_CONTEXT_WINDOW", default_value_t = 200_000)]
    context_window: u64,

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
        .init();
    let cli: &'static Cli = Box::leak(Box::new(Cli::parse()));

    let provider: Arc<dyn Provider> = if cli.mock {
        Arc::new(provider::mock::MockProvider::default())
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

    let workdir: &'static Path = Box::leak(
        cli.workdir
            .canonicalize()
            .with_context(|| format!("--workdir {}", cli.workdir.display()))?
            .into_boxed_path(),
    );
    let system: &'static str = Box::leak(build_system_prompt(workdir).into_boxed_str());

    let cfg = AgentConfig {
        model: &cli.model,
        system,
        effort: cli.effort,
        context_window: cli.context_window,
    };

    let agent = Agent {
        provider: provider.clone(),
        cfg,
        tools: Arc::new(Registry::default_set()),
        workdir,
    };

    let id = match &cli.session {
        Some(id) => uuid::Uuid::parse_str(id)
            .with_context(|| format!("--session {id}: not a UUID"))?
            .to_string(),
        None => uuid::Uuid::new_v4().to_string(),
    };
    let mut session = ChliveSession::new(&id, &cli.chlive_url, &cli.kafka)?;
    info!(
        session = id,
        chlive = cli.chlive_url,
        kafka = cli.kafka,
        provider = provider.name(),
        model = cfg.model,
        workdir = %workdir.display(),
        "running the session"
    );
    tokio::select! {
        r = agent.run(&mut session) => r?,
        _ = tokio::signal::ctrl_c() => info!("shutting down"),
    }
    Ok(())
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
