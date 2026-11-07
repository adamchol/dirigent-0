//! dirigent's command line. It signs you in to a dirigent (with Google, as the web does) and
//! links the accounts your sessions' model calls are paid with: a ChatGPT plan, whose sign-in
//! comes back to a listener only a process on your machine can have.

mod api;
mod credentials;
mod google;
mod loopback;
mod oauth;

use std::time::Duration;

use anyhow::Context;
use clap::builder::styling::{AnsiColor, Styles};
use clap::{Parser, Subcommand, ValueEnum};
use serde::Deserialize;
use serde_json::json;

use crate::api::Api;
use crate::credentials::{Auth, Credentials};
use crate::loopback::Loopback;

const HELP_STYLES: Styles = Styles::styled()
    .header(AnsiColor::BrightMagenta.on_default().bold())
    .usage(AnsiColor::BrightMagenta.on_default().bold())
    .literal(AnsiColor::BrightCyan.on_default())
    .placeholder(AnsiColor::BrightYellow.on_default())
    .valid(AnsiColor::BrightGreen.on_default())
    .invalid(AnsiColor::BrightRed.on_default())
    .error(AnsiColor::BrightRed.on_default().bold());

#[derive(Parser)]
#[command(name = "dirigent", about = "dirigent's command line", styles = HELP_STYLES)]
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
    /// Link an account your sessions run on: signed in in your browser, held by dirigent.
    Link {
        account: Account,
        #[arg(long)]
        no_browser: bool,
    },
    /// Unlink an account: dirigent revokes its grant and forgets it.
    Unlink { account: Account },
    /// The models your ChatGPT plan has.
    Models,
}

#[derive(Clone, Copy, ValueEnum)]
enum Account {
    /// A ChatGPT plan (Plus or Pro).
    Chatgpt,
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
        Command::Models => {
            let mut api = Api::signed_in(http)?;
            let models: Vec<Model> = api.get("/links/chatgpt/models").await?;
            for m in models {
                println!("{}\t{}", m.slug, m.display_name);
            }
        }
    }
    Ok(())
}
