//! `dirigent sync`: a session's workspace, mirrored into a folder here as the session's log has
//! it (see `dirigent_sync`), and kept so as its tools change it. The folder is a copy to read:
//! only the files the workspace has are ever written, and one changed here that the session
//! changes too is moved aside to `<path>.local` first. The commits are kept apart, in a
//! repository of the CLI's own (`~/.cache/dirigent/sync`), so the folder holds just the files.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use chlive_client::{ClientOptions, LiveClient};
use dirigent_protocol::rows::{EventRow, FILES_QUERY};
use dirigent_sync::{Bundle, Bundles, Checkout, Repo};
use futures::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::api::Api;

/// A session's names, newest last.
const TITLES_QUERY: &str = "select offset, data from session_events \
     where session_id = {session:UUID} and kind = 'titled' order by offset";

pub async fn sync(
    mut api: Api,
    session: &str,
    dir: Option<PathBuf>,
    chlive: Option<String>,
    once: bool,
) -> anyhow::Result<()> {
    api.get::<serde_json::Value>(&format!("/sessions/{session}"))
        .await
        .with_context(|| format!("reading the session {session}"))?;
    let url = match chlive {
        Some(url) => url,
        None => live(&api.creds.api)?,
    };
    let api = Arc::new(Mutex::new(api));
    let token = move || {
        let api = api.clone();
        async move { api.lock().await.chlive_token().await }
    };
    let client = LiveClient::new(ClientOptions::new(&url).credentials(token));
    let dir = match dir {
        Some(dir) => dir,
        None => PathBuf::from(folder(&client, session).await?),
    };
    std::fs::create_dir_all(&dir).with_context(|| format!("making {}", dir.display()))?;
    let dir = dir.canonicalize()?;
    let repo = Repo::open(&cache(session, &dir)?).await?;
    let mut status = Status::new(&dir);

    let (history, mut live) = client
        .query(FILES_QUERY)
        .param("session", session)
        .history::<EventRow>()
        .await
        .context("reading the session's workspace")?;
    let mut bundles = Bundles::default();
    let mut last = None;
    for row in history.iter() {
        if let Some(bundle) = whole(&mut bundles, row) {
            if fetch(&repo, &bundle, &mut status).await {
                last = Some(bundle.commit);
            }
        }
    }
    match &last {
        Some(commit) => checkout(&repo, &dir, commit, &mut status).await?,
        None => status.note("the session hasn't mirrored its workspace yet"),
    }
    if once {
        status.done();
        return Ok(());
    }
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            row = live.next() => {
                let row = match row {
                    Some(Ok(row)) => row,
                    Some(Err(e)) => anyhow::bail!("following the session: {e}"),
                    None => anyhow::bail!("chlive closed the session's log"),
                };
                if let Some(bundle) = whole(&mut bundles, &row) {
                    if fetch(&repo, &bundle, &mut status).await {
                        checkout(&repo, &dir, &bundle.commit, &mut status).await?;
                    }
                }
            }
            _ = tick.tick() => status.draw(),
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    status.done();
    Ok(())
}

/// The bundle `row` completes, if it does.
fn whole(bundles: &mut Bundles, row: &EventRow) -> Option<Bundle> {
    bundles.push(&row.event().ok()?)
}

/// Fetched, or said why not (and left out: what's made on it can't be fetched either).
async fn fetch(repo: &Repo, bundle: &Bundle, status: &mut Status) -> bool {
    match repo.fetch(bundle).await {
        Ok(()) => true,
        Err(e) => {
            status.note(&format!("skipped a change: {e:#}"));
            false
        }
    }
}

async fn checkout(
    repo: &Repo,
    dir: &Path,
    commit: &str,
    status: &mut Status,
) -> anyhow::Result<()> {
    let Checkout { changed, moved } = repo.checkout(dir, commit, true).await?;
    for path in &moved {
        status.note(&format!(
            "{} was changed here and by the session: yours is {}.local",
            path.display(),
            path.display()
        ));
    }
    status.synced(changed.len());
    Ok(())
}

/// chlive's WebSocket, beside control's API at the web's `/api`: its `/live`.
fn live(api: &str) -> anyhow::Result<String> {
    let base = api.strip_suffix("/api").context(
        "signed in to control's own API, so chlive's address isn't known: give it with --chlive",
    )?;
    let base = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        anyhow::bail!("{api} isn't an http(s) address");
    };
    Ok(format!("{base}/live"))
}

/// The folder a session goes in, unless one's given: `./<its title, as a slug>`, or its id's
/// first part while it has none.
async fn folder(client: &LiveClient, session: &str) -> anyhow::Result<String> {
    #[derive(Deserialize)]
    struct Row {
        data: String,
    }
    #[derive(Deserialize)]
    struct Titled {
        title: String,
    }
    let rows = client
        .query(TITLES_QUERY)
        .param("session", session)
        .complete()
        .fetch::<Row>()
        .await
        .context("reading the session's title")?;
    let title = rows
        .iter()
        .rev()
        .find_map(|r| serde_json::from_str::<Titled>(&r.data).ok())
        .map(|t| slug(&t.title))
        .filter(|s| !s.is_empty());
    Ok(title.unwrap_or_else(|| session.chars().take(8).collect()))
}

fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-')
        .chars()
        .take(48)
        .collect::<String>()
        .trim_end_matches('-')
        .into()
}

/// The repository a session's mirror in `dir` keeps its commits in: one per folder.
fn cache(session: &str, dir: &Path) -> anyhow::Result<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME").context("no $HOME")?).join(".cache"),
    };
    let hash = Sha256::digest(dir.as_os_str().as_encoded_bytes());
    let hash: String = hash.iter().take(6).map(|b| format!("{b:02x}")).collect();
    Ok(base
        .join("dirigent")
        .join("sync")
        .join(format!("{session}-{hash}")))
}

/// The status line on stderr: redrawn in place on a terminal, a line per sync otherwise.
struct Status {
    dir: String,
    tty: bool,
    last: Option<(Instant, usize)>,
}

impl Status {
    fn new(dir: &Path) -> Status {
        let dir = std::env::current_dir()
            .ok()
            .and_then(|cwd| {
                dir.strip_prefix(cwd)
                    .ok()
                    .map(|d| format!("./{}", d.display()))
            })
            .unwrap_or_else(|| dir.display().to_string());
        Status {
            dir,
            tty: std::io::stderr().is_terminal(),
            last: None,
        }
    }

    fn synced(&mut self, changed: usize) {
        self.last = Some((Instant::now(), changed));
        if self.tty {
            self.draw();
        } else {
            eprintln!("synced {} · {}", self.dir, files(changed));
        }
    }

    fn note(&mut self, line: &str) {
        if self.tty {
            eprint!("\r\x1b[2K");
        }
        eprintln!("{line}");
        self.draw();
    }

    fn draw(&self) {
        let Some((at, changed)) = self.last else {
            return;
        };
        if !self.tty {
            return;
        }
        let ago = at.elapsed().as_secs();
        let ago = match ago {
            0 => "just now".to_string(),
            s if s < 60 => format!("{s}s ago"),
            s if s < 3600 => format!("{}m ago", s / 60),
            s => format!("{}h ago", s / 3600),
        };
        eprint!(
            "\r\x1b[2Ksynced {} · {} {ago} · watching (ctrl-c stops)",
            self.dir,
            files(changed)
        );
        let _ = std::io::stderr().flush();
    }

    fn done(&self) {
        if self.tty {
            eprint!("\r\x1b[2K");
            if let Some((_, changed)) = self.last {
                eprintln!("synced {} · {}", self.dir, files(changed));
            }
        }
    }
}

fn files(n: usize) -> String {
    match n {
        0 => "up to date".into(),
        1 => "1 file changed".into(),
        n => format!("{n} files changed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chlive_is_beside_the_webs_api() {
        assert_eq!(
            live("https://d.example.com/api").unwrap(),
            "wss://d.example.com/live"
        );
        assert_eq!(
            live("http://127.0.0.1:3000/api").unwrap(),
            "ws://127.0.0.1:3000/live"
        );
        assert!(live("http://127.0.0.1:8090").is_err());
    }

    #[test]
    fn a_title_makes_a_folder_name() {
        assert_eq!(slug("Fix the login redirect!"), "fix-the-login-redirect");
        assert_eq!(slug("  Ünïcode  & co. "), "ünïcode-co");
        assert_eq!(slug("!!!"), "");
    }
}
