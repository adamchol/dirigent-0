//! Terminal client for a dirigent session: the session's log comes from chlive, and what the
//! user types is appended to it through Kafka.

mod app;

use std::time::Duration;

use anyhow::Context;
use chlive_client::{Change, ClientOptions, Kind, LiveClient, Status, Subscription};
use clap::Parser;
use crossterm::event::{Event as TermEvent, EventStream};
use dirigent_protocol::rows::{EventRow, EVENTS, SESSION_QUERY, USER};
use dirigent_protocol::Event;
use futures::StreamExt;
use ratatui::DefaultTerminal;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::ClientConfig;

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

    #[arg(long, env = "DIRIGENT_KAFKA", default_value = "127.0.0.1:9092")]
    kafka: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let session = uuid::Uuid::parse_str(&cli.session)
        .with_context(|| format!("--session {}: not a UUID", cli.session))?
        .to_string();
    let producer: FutureProducer = ClientConfig::new()
        .set("bootstrap.servers", &cli.kafka)
        .set("enable.idempotence", "true")
        .create()
        .context("creating the Kafka producer")?;
    let client = LiveClient::new(ClientOptions::new(&cli.chlive_url));
    let log = client
        .query(SESSION_QUERY)
        .param("session", &session)
        .raw::<EventRow>();

    let mut terminal = ratatui::init();
    let app = App::new(session.clone(), cli.chlive_url);
    let result = run(&mut terminal, app, &session, producer, log).await;
    ratatui::restore();
    result
}

async fn run(
    terminal: &mut DefaultTerminal,
    mut app: App,
    session: &str,
    producer: FutureProducer,
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
                        Some(Action::Send(text)) => Event::UserMessage { text },
                        Some(Action::Interrupt) => match app.turn() {
                            Some(turn) => Event::UserInterrupt { turn },
                            None => {
                                app.status = Some("no turn is running".into());
                                continue;
                            }
                        },
                    };
                    let id = uuid::Uuid::new_v4().to_string();
                    let row = EventRow::new(session, id, USER, &event);
                    let payload = serde_json::to_string(&row).expect("rows serialize");
                    let record = FutureRecord::to(EVENTS).key(session).payload(&payload);
                    app.status = match producer.send(record, Duration::from_secs(10)).await {
                        Ok(_) => None,
                        Err((e, _)) => Some(format!("could not send: {e}")),
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
