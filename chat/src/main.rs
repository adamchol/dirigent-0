//! Terminal client for a dirigent session it attaches to (sessions are made through control):
//! the session's log comes from chlive, and what the user types is inserted into it through
//! chlive, as `--user`, who needs access to the session: with the credentials control signs
//! for them, in its `declared` auth (dev).

mod app;

use anyhow::Context;
use chlive_client::{Change, ClientOptions, Kind, LiveClient, Status, Subscription};
use clap::Parser;
use crossterm::event::{Event as TermEvent, EventStream};
use dirigent_protocol::rows::{EventRow, EVENTS, SESSION_QUERY};
use dirigent_protocol::Event;
use futures::StreamExt;
use ratatui::DefaultTerminal;

use crate::app::{Action, App};

#[derive(Parser)]
#[command(
    name = "dirigent-chat",
    about = "terminal client for a dirigent session"
)]
struct Cli {
    /// The session (a UUID).
    #[arg(long, env = "DIRIGENT_SESSION")]
    session: String,

    #[arg(long, env = "CHLIVE_URL", default_value = "ws://127.0.0.1:8080/live")]
    chlive_url: String,

    /// Who the user is, to chlive (a subject with access to the session).
    #[arg(long, env = "DIRIGENT_USER", default_value = "user:dev")]
    user: String,

    /// Control's HTTP API, which signs the user's chlive credentials.
    #[arg(
        long,
        env = "DIRIGENT_CONTROL_HTTP",
        default_value = "http://127.0.0.1:8090"
    )]
    control_http: String,

    /// Also show the player's own events (attaching, the workspace, checkpoints), dimmed.
    /// Failures always show.
    #[arg(long, env = "DIRIGENT_CHAT_EVENTS")]
    events: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let session = uuid::Uuid::parse_str(&cli.session)
        .with_context(|| format!("--session {}: not a UUID", cli.session))?
        .to_string();
    let (control, user) = (cli.control_http.clone(), cli.user.clone());
    let token = move || token(control.clone(), user.clone());
    let client = LiveClient::new(ClientOptions::new(&cli.chlive_url).credentials(token));
    let log = client
        .query(SESSION_QUERY)
        .param("session", &session)
        .raw::<EventRow>();

    let mut terminal = ratatui::init();
    let app = App::new(session.clone(), cli.chlive_url, cli.events);
    let result = run(&mut terminal, app, &session, &client, log).await;
    ratatui::restore();
    result
}

/// `user`'s chlive credential, from control (`POST /token`).
async fn token(control: String, user: String) -> Result<String, String> {
    let got: serde_json::Value = reqwest::Client::new()
        .post(format!("{}/token", control.trim_end_matches('/')))
        .header("dirigent-user", user)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("control: {e}"))?
        .json()
        .await
        .map_err(|e| format!("control's token: {e}"))?;
    got["token"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| "control sent no token".into())
}

async fn run(
    terminal: &mut DefaultTerminal,
    mut app: App,
    session: &str,
    client: &LiveClient,
    mut log: Subscription<EventRow>,
) -> anyhow::Result<()> {
    let mut keys = EventStream::new();
    let mut log_open = true;
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        tokio::select! {
            key = keys.next() => match key {
                Some(Ok(TermEvent::Key(key))) => {
                    let event = match app.on_key(key) {
                        None => continue,
                        Some(Action::Quit) => return Ok(()),
                        Some(Action::Send(text)) => Event::UserMessage { text, via: None },
                        Some(Action::Interrupt) => match app.turn() {
                            Some(turn) => Event::UserInterrupt { turn },
                            None => {
                                app.status = Some("no turn is running".into());
                                continue;
                            }
                        },
                    };
                    let id = uuid::Uuid::new_v4().to_string();
                    let row = EventRow::new(session, id, &event);
                    app.status = match client.insert(EVENTS, [row]).await {
                        Ok(_) => None,
                        Err(e) => Some(format!("could not send: {e}")),
                    };
                }
                Some(Ok(_)) => {} // resize etc.: redraw
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(()),
            },
            update = log.next(), if log_open => match update {
                Some(u) => match u.kind {
                    Kind::Snapshot => {
                        app.status = None;
                        app.reset();
                        for row in u.rows.iter() {
                            app.on_row(row);
                        }
                    }
                    Kind::Diff => {
                        for change in &u.changes {
                            if let Change::Insert { row, .. } = change {
                                app.on_row(row);
                            }
                        }
                    }
                    Kind::Disconnected => app.status = Some("chlive disconnected; reconnecting…".into()),
                    Kind::Error => {
                        if let Status::Error(e) = &u.status {
                            app.status = Some(format!("the log subscription failed: {e}"));
                        }
                    }
                    Kind::Subscribed | Kind::Resumed => {}
                },
                None => {
                    log_open = false;
                    app.status = Some("the log subscription ended · Ctrl-C quits".into());
                }
            },
        }
    }
}
