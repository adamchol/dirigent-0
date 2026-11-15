//! dirigent's command line. It signs you in to a dirigent (with Google, as the web does),
//! links the accounts your sessions' model calls are paid with (a ChatGPT plan, whose sign-in
//! comes back to a listener only a process on your machine can have; an Anthropic API key),
//! keeps the secrets their tools get, and mirrors a session's workspace into a folder here.

mod api;
mod credentials;
mod google;
mod host;
mod import;
mod loopback;
mod oauth;
mod sync;
mod version;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context;
use clap::builder::styling::{AnsiColor, Styles};
use clap::{Parser, Subcommand, ValueEnum};
use serde::Deserialize;
use serde_json::json;

use crate::api::Api;
use crate::credentials::{Auth, Credentials};
use crate::loopback::Loopback;

/// The hidden first argument that makes this binary a player (`dirigent host` runs its players so).
pub const PLAYER: &str = "__player";

const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::BrightMagenta.on_default().bold())
    .usage(AnsiColor::BrightMagenta.on_default().bold())
    .literal(AnsiColor::BrightCyan.on_default())
    .placeholder(AnsiColor::BrightYellow.on_default())
    .valid(AnsiColor::BrightGreen.on_default())
    .invalid(AnsiColor::BrightRed.on_default())
    .error(AnsiColor::BrightRed.on_default().bold());

#[derive(Parser)]
#[command(
    name = "dirigent",
    about = "dirigent's command line",
    version = dirigent_player::VERSION,
    styles = HELP_STYLES
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in to a dirigent: with Google, in your browser (or, in its dev auth, as `--as`).
    Login {
        /// The dirigent's address, the web's (its API is its `/api`).
        url: String,
        /// The address is control's own API, not the web's.
        #[arg(long)]
        control: bool,
        /// In a dirigent's `declared` auth (dev): who to be.
        #[arg(long = "as", default_value = "user:dev")]
        subject: String,
        /// Print the sign-in's address rather than opening a browser.
        #[arg(long)]
        no_browser: bool,
    },
    /// Sign out: the Google sign-in is revoked and forgotten.
    Logout,
    /// Who you are to the dirigent, and the accounts you've linked.
    Whoami,
    /// This CLI's version, and what the dirigent you're signed in to runs: control, the
    /// players' VM image and system, Postgres, ClickHouse, the install's pods (in Kubernetes)
    /// and your connected hosts. For bug reports.
    Version {
        /// As JSON: `{"cli", "about" (control's `GET /about`), "hosts"}`.
        #[arg(long)]
        json: bool,
    },
    /// Link an account your sessions run on, held by dirigent: a ChatGPT plan, signed in in your
    /// browser; an Anthropic API key, typed (hidden) or on stdin.
    Link {
        account: Account,
        #[arg(long)]
        no_browser: bool,
    },
    /// Unlink an account: dirigent revokes its grant (or forgets the key).
    Unlink { account: Account },
    /// The secrets your sessions' tools get, by name: for all your sessions, or one's
    /// (`--session`), which wins. Values are never shown again. `GITHUB_TOKEN` is git's for
    /// GitHub too: a session on a private repository clones it with it.
    Secret {
        #[command(subcommand)]
        command: SecretCommand,
    },
    /// The models your ChatGPT plan has.
    Models,
    /// Mirror a session's workspace into a folder here, kept as the session changes it (to
    /// read: a file you change that the session changes too is moved aside to `<file>.local`).
    /// Needs git.
    Sync {
        /// The session's id.
        session: String,
        /// The folder; by default `./<the session's title>`.
        dir: Option<PathBuf>,
        /// chlive's WebSocket, when signed in to control's own API (`login --control`).
        #[arg(long, env = "DIRIGENT_CHLIVE")]
        chlive: Option<String>,
        /// Sync once, then exit, rather than keep following the session.
        #[arg(long)]
        once: bool,
    },
    /// This machine as a place where your sessions run: a daemon connected to dirigent as you,
    /// running a player for each session you place on it (the web's session menu, or
    /// `PUT /sessions/{id}/environment`), in a directory of its own under the host's roots. Its
    /// config: `~/.config/dirigent/host.toml` (url, name, roots, slots, labels, environment).
    Host {
        #[command(subcommand)]
        command: HostCommand,
    },
    /// Make another agent's session here a dirigent session that goes on from where it was:
    /// its conversation (thinking kept to read, secrets redacted), on the project's git remote
    /// with the files as they are here. Not started: your next message starts it.
    Import {
        agent: ImportAgent,
        /// Its id (or its transcript's path); by default the last one of this directory.
        session: Option<String>,
        /// The project, when it isn't where the session worked.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// The model it goes on with; by default the one it had.
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<ImportEffort>,
        /// Who its model calls are paid to; by default the agent's (Claude Code: Anthropic;
        /// Codex: your ChatGPT plan).
        #[arg(long)]
        provider: Option<ImportProvider>,
        /// Show what would be imported, and import nothing.
        #[arg(long)]
        dry_run: bool,
        /// Import it into a new session, though it was imported before.
        #[arg(long)]
        again: bool,
        /// Keep what looks like secrets as it is.
        #[arg(long)]
        no_redact: bool,
        /// chlive's WebSocket, when signed in to control's own API (`login --control`).
        #[arg(long, env = "DIRIGENT_CHLIVE")]
        chlive: Option<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Account {
    /// A ChatGPT plan (Plus or Pro).
    Chatgpt,
    /// An Anthropic API key, which your Anthropic sessions run on rather than the install's.
    Anthropic,
}

#[derive(Clone, Copy, ValueEnum)]
enum ImportAgent {
    /// Claude Code (~/.claude/projects).
    ClaudeCode,
    /// Codex (~/.codex/sessions).
    Codex,
}

#[derive(Clone, Copy, ValueEnum)]
enum ImportEffort {
    Low,
    Medium,
    High,
    Max,
}

#[derive(Clone, Copy, ValueEnum)]
enum ImportProvider {
    Anthropic,
    Chatgpt,
}

#[derive(Subcommand)]
enum HostCommand {
    /// Run the host here, until it's stopped (what the service runs). As you, never root.
    Run,
    /// Install the host as your service and start it: a launchd agent on macOS (in your login
    /// session, where Xcode's keychain and the Simulator are; a locked screen doesn't stop it),
    /// a systemd user unit on Linux (lingering: from boot). As you, never with sudo.
    Install,
    /// Stop the service and remove it (players running go on until their sessions stop).
    Uninstall,
    /// What of the host runs here, and where: its service, whether control has it connected,
    /// the sessions running here and their directories, the last problems in its log.
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum SecretCommand {
    /// Set a secret: its value typed (hidden) or on stdin.
    Set {
        /// Its name, the variable the tools get (e.g. GITHUB_TOKEN).
        name: String,
        /// Only this session's (its id); without, all your sessions'.
        #[arg(long)]
        session: Option<String>,
    },
    /// The names of the secrets.
    List {
        #[arg(long)]
        session: Option<String>,
    },
    /// Remove a secret.
    Rm {
        name: String,
        #[arg(long)]
        session: Option<String>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum AuthMode {
    Declared,
    Oidc { cli: Option<CliClient> },
}

#[derive(Deserialize)]
struct CliClient {
    client_id: String,
    client_secret: String,
}

#[derive(Deserialize)]
struct Me {
    subject: String,
    chatgpt: Option<Link>,
    anthropic: Option<Hint>,
}

#[derive(Deserialize)]
struct Hint {
    hint: String,
}

#[derive(Deserialize)]
struct Link {
    email: String,
    broken: Option<String>,
}

#[derive(Deserialize)]
struct Started {
    url: String,
    state: String,
}

#[derive(Deserialize)]
struct Linked {
    email: String,
}

#[derive(Deserialize)]
struct Model {
    slug: String,
    display_name: String,
}

#[tokio::main]
async fn main() {
    // a player `dirigent host` runs, in a process of its own: its arguments are the player's
    let mut args = std::env::args_os();
    if args.nth(1).is_some_and(|a| a == PLAYER) {
        let player =
            dirigent_player::Args::parse_from(std::iter::once("player".into()).chain(args));
        if let Err(e) = dirigent_player::run(player).await {
            eprintln!("player: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(e) = run(Cli::parse()).await {
        eprintln!("dirigent: {e:#}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(60))
        .build()?;
    match cli.command {
        Command::Login {
            url,
            control,
            subject,
            no_browser,
        } => {
            let url = url.trim_end_matches('/');
            let api = if control {
                url.to_string()
            } else {
                format!("{url}/api")
            };
            let mode: AuthMode = http
                .get(format!("{api}/auth"))
                .send()
                .await
                .with_context(|| format!("reaching {api}"))?
                .error_for_status()?
                .json()
                .await
                .with_context(|| format!("{api}/auth isn't a dirigent's"))?;
            let auth = match mode {
                AuthMode::Declared => Auth::Declared { subject },
                AuthMode::Oidc { cli: None } => anyhow::bail!(
                    "this dirigent has no sign-in for its CLI (control's --oidc-cli-client-id)"
                ),
                AuthMode::Oidc {
                    cli:
                        Some(CliClient {
                            client_id,
                            client_secret,
                        }),
                } => {
                    let (id_token, refresh_token) =
                        google::sign_in(&http, &client_id, &client_secret, !no_browser).await?;
                    Auth::Oidc {
                        client_id,
                        client_secret,
                        id_token,
                        refresh_token,
                    }
                }
            };
            let path = credentials::path()?;
            credentials::save(&path, &Credentials { api, auth })?;
            let mut api = Api::signed_in(http)?;
            let me: Me = api.get("/me").await?;
            println!("Signed in as {}", me.subject);
        }
        Command::Logout => {
            let path = credentials::path()?;
            if let Some(Credentials {
                auth:
                    Auth::Oidc {
                        refresh_token: Some(refresh),
                        ..
                    },
                ..
            }) = credentials::load(&path)?
            {
                google::revoke(&http, &refresh).await?;
            }
            credentials::remove(&path)?;
            println!("Signed out");
        }
        Command::Whoami => {
            let mut api = Api::signed_in(http)?;
            let me: Me = api.get("/me").await?;
            println!("{} at {}", me.subject, api.creds.api);
            let about: version::About = api.get("/about").await?;
            println!(
                "dirigent {}, this CLI {}",
                about.control.version,
                dirigent_player::VERSION
            );
            match me.chatgpt {
                None => println!("ChatGPT: not linked (dirigent link chatgpt)"),
                Some(Link {
                    email,
                    broken: None,
                }) => println!("ChatGPT: {email}"),
                Some(Link {
                    email,
                    broken: Some(why),
                }) => println!("ChatGPT: {email}, broken: {why} (dirigent link chatgpt)"),
            }
            match me.anthropic {
                Some(Hint { hint }) => println!("Anthropic: your key …{hint}"),
                None => println!("Anthropic: no key (dirigent link anthropic)"),
            }
        }
        Command::Link {
            account: Account::Chatgpt,
            no_browser,
        } => {
            let mut api = Api::signed_in(http)?;
            let lb = Loopback::bind().await?;
            let started: Started = api
                .post("/links/chatgpt/start", &json!({ "port": lb.port }))
                .await?;
            loopback::open(&started.url, !no_browser);
            let query = lb
                .wait(
                    &started.state,
                    Duration::from_secs(300),
                    "ChatGPT is linked to dirigent. You can close this tab.",
                )
                .await?;
            if let Some(error) = query.get("error") {
                anyhow::bail!("ChatGPT wasn't linked: {error}");
            }
            let linked: Linked = api.post("/links/chatgpt/finish", &query).await?;
            println!("Linked ChatGPT: {}", linked.email);
        }
        Command::Unlink {
            account: Account::Chatgpt,
        } => {
            let mut api = Api::signed_in(http)?;
            api.delete("/links/chatgpt").await?;
            println!("Unlinked ChatGPT");
        }
        Command::Link {
            account: Account::Anthropic,
            ..
        } => {
            let mut api = Api::signed_in(http)?;
            let key = hidden("Anthropic API key: ")?;
            let saved: Hint = api.put("/links/anthropic", &json!({ "key": key })).await?;
            println!(
                "Anthropic: your key …{} (your sessions run on it)",
                saved.hint
            );
        }
        Command::Unlink {
            account: Account::Anthropic,
        } => {
            let mut api = Api::signed_in(http)?;
            api.delete("/links/anthropic").await?;
            println!("Removed your Anthropic key");
        }
        Command::Secret { command } => {
            let mut api = Api::signed_in(http)?;
            let base = |session: &Option<String>| match session {
                Some(s) => format!("/sessions/{s}/secrets"),
                None => "/secrets".into(),
            };
            match command {
                SecretCommand::Set { name, session } => {
                    let value = hidden(&format!("{name}: "))?;
                    let path = format!("{}/{name}", base(&session));
                    api.put_empty(&path, &json!({ "value": value })).await?;
                    println!("Set {name}");
                }
                SecretCommand::List { session } => {
                    let names: Vec<String> = api.get(&base(&session)).await?;
                    for name in names {
                        println!("{name}");
                    }
                }
                SecretCommand::Rm { name, session } => {
                    api.delete(&format!("{}/{name}", base(&session))).await?;
                    println!("Removed {name}");
                }
            }
        }
        Command::Version { json } => {
            let cli = dirigent_player::VERSION;
            let fetched = async {
                let mut api = Api::signed_in(http)?;
                let (about, hosts) = version::fetch(&mut api).await?;
                anyhow::Ok((api.creds.api, about, hosts))
            };
            match (fetched.await, json) {
                (Ok((_, about, hosts)), true) => {
                    let all = json!({ "cli": cli, "about": about, "hosts": hosts });
                    println!("{}", serde_json::to_string_pretty(&all)?);
                }
                (Ok((url, about, hosts)), false) => {
                    print!("{}", version::render(cli, &url, &about, &hosts))
                }
                // the CLI's own, whatever the dirigent's
                (Err(e), _) => {
                    println!("cli {cli}");
                    return Err(e);
                }
            }
        }
        Command::Models => {
            let mut api = Api::signed_in(http)?;
            let models: Vec<Model> = api.get("/links/chatgpt/models").await?;
            for m in models {
                println!("{}\t{}", m.slug, m.display_name);
            }
        }
        Command::Sync {
            session,
            dir,
            chlive,
            once,
        } => sync::sync(Api::signed_in(http)?, &session, dir, chlive, once).await?,
        Command::Host { command } => {
            use host::service;
            match command {
                HostCommand::Run => {
                    tracing_subscriber::fmt()
                        .with_env_filter(
                            tracing_subscriber::EnvFilter::try_from_default_env()
                                .unwrap_or_else(|_| "dirigent=info".into()),
                        )
                        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stdout()))
                        .init();
                    let config = host::Config::load(&host::config_path()?)?;
                    host::run(Api::signed_in(http)?, config).await?
                }
                HostCommand::Install => service::install().await?,
                HostCommand::Uninstall => service::uninstall().await?,
                HostCommand::Status { json } => {
                    let status = host::status::status(http).await?;
                    match json {
                        true => println!("{}", serde_json::to_string_pretty(&status)?),
                        false => print!("{}", host::status::render(&status)),
                    }
                }
            }
        }
        Command::Import {
            agent,
            session,
            dir,
            model,
            effort,
            provider,
            dry_run,
            again,
            no_redact,
            chlive,
        } => {
            use dirigent_protocol::control::{Effort, Provider};
            let ask = import::Import {
                agent: match agent {
                    ImportAgent::ClaudeCode => import::Agent::ClaudeCode,
                    ImportAgent::Codex => import::Agent::Codex,
                },
                session,
                dir,
                model,
                effort: effort.map(|e| match e {
                    ImportEffort::Low => Effort::Low,
                    ImportEffort::Medium => Effort::Medium,
                    ImportEffort::High => Effort::High,
                    ImportEffort::Max => Effort::Max,
                }),
                provider: provider.map(|p| match p {
                    ImportProvider::Anthropic => Provider::Anthropic,
                    ImportProvider::Chatgpt => Provider::Chatgpt,
                }),
                dry_run,
                again,
                redact: !no_redact,
                chlive,
            };
            import::import(Api::signed_in(http)?, ask).await?
        }
    }
    Ok(())
}

/// A secret: typed without echo on a terminal, else stdin's (without its last line ending).
fn hidden(prompt: &str) -> anyhow::Result<String> {
    use std::io::{IsTerminal, Read};
    let value = if std::io::stdin().is_terminal() {
        rpassword::prompt_password(prompt)?
    } else {
        let mut value = String::new();
        std::io::stdin().read_to_string(&mut value)?;
        value.trim_end_matches(['\n', '\r']).to_string()
    };
    anyhow::ensure!(!value.is_empty(), "nothing was given");
    Ok(value)
}
