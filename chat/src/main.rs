//! Temporary terminal client for a dirigent player's gRPC session.

mod app;

pub mod pb {
    tonic::include_proto!("player.v1");
}

use anyhow::{anyhow, Context};
use clap::Parser;
use crossterm::event::{Event as TermEvent, EventStream};
use futures::StreamExt;
use ratatui::DefaultTerminal;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Streaming;

use crate::app::{Action, App};
use crate::pb::player_client::PlayerClient;

#[derive(Parser)]
#[command(
    name = "dirigent-chat",
    about = "terminal client for a dirigent player"
)]
struct Cli {
    #[arg(long, env = "DIRIGENT_ADDR", default_value = "http://127.0.0.1:8787")]
    addr: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    // Connect before taking over the terminal, so a refusal prints normally.
    let mut client = PlayerClient::connect(cli.addr.clone())
        .await
        .with_context(|| format!("could not connect to {}", cli.addr))?;
    let (input, rx) = mpsc::channel::<pb::Input>(32);
    let events = client
        .session(ReceiverStream::new(rx))
        .await
        .map_err(|s| anyhow!("session refused ({:?}): {}", s.code(), s.message()))?
        .into_inner();

    let mut terminal = ratatui::init();
    let result = run(&mut terminal, App::new(cli.addr), input, events).await;
    ratatui::restore();
    result
}

async fn run(
    terminal: &mut DefaultTerminal,
    mut app: App,
    input: mpsc::Sender<pb::Input>,
    mut events: Streaming<pb::Event>,
) -> anyhow::Result<()> {
    let mut keys = EventStream::new();
    let mut connected = true;
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        tokio::select! {
            key = keys.next() => match key {
                Some(Ok(TermEvent::Key(key))) => {
                    let kind = match app.on_key(key) {
                        None => continue,
                        Some(Action::Quit) => return Ok(()),
                        Some(Action::Send(text)) => pb::input::Kind::Message(pb::UserMessage { text }),
                        Some(Action::Interrupt) => pb::input::Kind::Interrupt(pb::Interrupt {}),
                    };
                    if !connected || input.send(pb::Input { kind: Some(kind) }).await.is_err() {
                        app.status = Some("not connected · Ctrl-C quits".into());
                    }
                }
                Some(Ok(_)) => {} // resize etc.: redraw
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(()),
            },
            ev = events.message(), if connected => match ev {
                Ok(Some(ev)) => app.on_event(ev),
                Ok(None) => {
                    connected = false;
                    app.status = Some("player closed the session · Ctrl-C quits".into());
                }
                Err(status) => {
                    connected = false;
                    app.status = Some(format!("stream error: {} · Ctrl-C quits", status.message()));
                }
            },
        }
    }
}
