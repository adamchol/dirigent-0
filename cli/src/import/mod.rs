//! `dirigent import`: another agent's session, read from its transcript here (Claude Code's,
//! Codex's), made a dirigent session that goes on from where it was. Its conversation is written
//! into the new session's log as the session's importer (`import:<id>`, whom control lets write
//! a session no player has held yet), as a player would have written it: inputs and the turns
//! they started, replies, tool results, compactions. Thinking is kept to read but never sent to
//! a model again (it's bound to the conversation it was thought in), and what a tool's output
//! had of secrets is redacted. The workspace is the project's git remote, with the files as they
//! are here on top (the log's `FilesChanged`, which the session's first player checks out).

pub mod claude_code;
pub mod codex;
mod redact;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Context;
use chlive_client::{ClientOptions, LiveClient};
use dirigent_protocol::control::{Effort, Provider, SessionSpec, Source};
use dirigent_protocol::rows::{EventRow, EVENTS};
use dirigent_protocol::{ContentBlock, Event, StopReason, TurnEndReason, Usage};
use dirigent_sync::{Repo, Workspace};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::api::Api;

/// The most rows, and bytes of them, one insert takes.
const MAX_ROWS: usize = 1000;
const MAX_BYTES: usize = 4 << 20;

/// A string longer than this is clipped in its middle, so each row stays well within Kafka's
/// message size (1 MB by default).
const MAX_STRING: usize = 200_000;

/// What's asked of an import.
pub struct Import {
    pub agent: Agent,
    /// The session's id, or its transcript's path; none: the last one of this directory.
    pub session: Option<String>,
    /// The project, when not where the session worked.
    pub dir: Option<PathBuf>,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    pub provider: Option<Provider>,
    pub dry_run: bool,
    /// A new session even if this one was imported before.
    pub again: bool,
    pub redact: bool,
    pub chlive: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    ClaudeCode,
    Codex,
}

impl Agent {
    /// Its name in [`Event::Imported`].
    fn name(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "claude-code",
            Agent::Codex => "codex",
        }
    }

    fn provider(self) -> Provider {
        match self {
            Agent::ClaudeCode => Provider::Anthropic,
            Agent::Codex => Provider::Chatgpt,
        }
    }
}

/// A session of another agent's, as dirigent's events.
#[derive(Debug, Default)]
pub struct Conversation {
    /// Its id there.
    pub session: String,
    /// Where it worked first.
    pub cwd: PathBuf,
    /// When it began and last went on (RFC 3339).
    pub first: String,
    pub last: String,
    /// The model of its last reply.
    pub model: Option<String>,
    /// Its title, and whether it was given by hand (held).
    pub title: Option<(String, bool)>,
    pub events: Vec<(String, Event)>,
}

/// A conversation as it's read, turn by turn: each input starts a turn, or joins the open one.
pub struct Log {
    pub events: Vec<(String, Event)>,
    in_turn: bool,
    ids: Box<dyn FnMut() -> String>,
}

impl Default for Log {
    fn default() -> Self {
        Log::with_ids(Box::new(|| uuid(rand::random(), 4)))
    }
}

impl Log {
    pub fn with_ids(ids: Box<dyn FnMut() -> String>) -> Self {
        Log {
            events: Vec::new(),
            in_turn: false,
            ids,
        }
    }

    pub fn push(&mut self, event: Event) -> String {
        let id = (self.ids)();
        self.events.push((id.clone(), event));
        id
    }

    /// The user's message, starting a turn (and ending the open one, which the transcript
    /// didn't).
    pub fn prompt(&mut self, text: String) {
        self.end(TurnEndReason::Completed);
        self.input(text);
    }

    /// An input: it joins the open turn (as a steer), or starts one.
    pub fn input(&mut self, text: String) {
        let input = self.push(Event::UserMessage { text, via: None });
        match self.in_turn {
            true => self.push(Event::Steered { input }),
            false => self.push(Event::TurnStarted { input }),
        };
        self.in_turn = true;
    }

    pub fn end(&mut self, reason: TurnEndReason) {
        if std::mem::replace(&mut self.in_turn, false) {
            self.push(Event::TurnEnded {
                reason,
                interrupt: None,
            });
        }
    }

    /// The last turn, ended as the transcript left it: completed after a reply that ended it,
    /// interrupted otherwise.
    pub fn finish(&mut self) {
        let done = self.events.iter().rev().find_map(|(_, e)| match e {
            Event::AssistantMessage { stop_reason, .. } => {
                Some(*stop_reason == StopReason::EndTurn)
            }
            Event::UserMessage { .. } | Event::ToolResult { .. } => Some(false),
            _ => None,
        });
        self.end(match done {
            Some(true) => TurnEndReason::Completed,
            _ => TurnEndReason::Aborted,
        });
    }

    /// A compaction: `summary` replaces what's before the event `first_kept`, or all of it.
    pub fn compacted(&mut self, summary: String, first_kept: Option<String>) {
        let id = (self.ids)();
        let first_kept = first_kept.unwrap_or_else(|| id.clone());
        let event = Event::Compacted {
            summary,
            first_kept,
            usage: Usage::default(),
        };
        self.events.push((id, event));
    }
}

/// The stop reason a reply had, by its API's name.
pub fn stop_reason(raw: Option<&str>, content: &[ContentBlock]) -> StopReason {
    match raw {
        Some("end_turn") | Some("stop_sequence") => StopReason::EndTurn,
        Some("tool_use") => StopReason::ToolUse,
        Some("max_tokens") => StopReason::MaxTokens,
        Some("refusal") => StopReason::Refusal,
        Some(other) => StopReason::Other { raw: other.into() },
        None if content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolCall { .. })) =>
        {
            StopReason::ToolUse
        }
        None => StopReason::EndTurn,
    }
}

/// A UUID's text from 16 bytes, as version `version` (4: random; 8: made of a name's hash).
pub fn uuid(bytes: [u8; 16], version: u8) -> String {
    let mut b = bytes;
    b[6] = (b[6] & 0x0f) | (version << 4);
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    )
}

/// The dirigent session `subject`'s import of `agent`'s session `session` is, the same each
/// time.
fn session_id(subject: &str, agent: Agent, session: &str) -> String {
    let hash = Sha256::digest(format!("{subject}\n{}\n{session}", agent.name()));
    uuid(hash[..16].try_into().expect("16 bytes"), 8)
}

/// Every string in an event the conversation has, for `f` to change.
fn strings(event: &mut Event, f: &mut dyn FnMut(&mut String)) {
    fn json(v: &mut Value, f: &mut dyn FnMut(&mut String)) {
        match v {
            Value::String(s) => f(s),
            Value::Array(a) => a.iter_mut().for_each(|v| json(v, f)),
            Value::Object(o) => o.values_mut().for_each(|v| json(v, f)),
            _ => {}
        }
    }
    match event {
        Event::UserMessage { text, .. } => f(text),
        Event::AssistantMessage { content, .. } => {
            for block in content {
                match block {
                    ContentBlock::Text { text } | ContentBlock::Thinking { text, .. } => f(text),
                    ContentBlock::ToolCall { input, .. } => json(input, f),
                    ContentBlock::ToolResult { content, .. } => f(content),
                }
            }
        }
        Event::ToolResult { content, .. } => f(content),
        Event::Compacted { summary, .. } => f(summary),
        _ => {}
    }
}

/// `s`, clipped in its middle to [`MAX_STRING`] bytes.
fn clip(s: &mut String) {
    if s.len() <= MAX_STRING {
        return;
    }
    let head = floor(s, MAX_STRING * 3 / 4);
    let tail = ceil(s, s.len() - MAX_STRING / 4);
    *s = format!(
        "{}\n… [{} characters left out on import] …\n{}",
        &s[..head],
        s[head..tail].chars().count(),
        &s[tail..]
    );
}

fn floor(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

/// Where the project is, and what it's cloned from: the top of the git repository `dir` is
/// in, its `origin`, and the branch checked out.
struct Project {
    dir: PathBuf,
    source: Option<Source>,
    /// Whether it's in git at all.
    git: bool,
}

async fn project(dir: &Path) -> Project {
    let git = |args: &[&str]| {
        let mut cmd = tokio::process::Command::new("git");
        cmd.current_dir(dir)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        async move {
            let out = cmd.output().await.ok()?;
            let line = String::from_utf8(out.stdout).ok()?.trim().to_string();
            (out.status.success() && !line.is_empty()).then_some(line)
        }
    };
    let Some(top) = git(&["rev-parse", "--show-toplevel"]).await else {
        return Project {
            dir: dir.to_path_buf(),
            source: None,
            git: false,
        };
    };
    let source = git(&["remote", "get-url", "origin"])
        .await
        .map(|url| Source::Git {
            url,
            reference: None,
        });
    let source = match (
        source,
        git(&["symbolic-ref", "--short", "-q", "HEAD"]).await,
    ) {
        (Some(Source::Git { url, .. }), branch) => Some(Source::Git {
            url,
            reference: branch,
        }),
        (source, _) => source,
    };
    Project {
        dir: PathBuf::from(top),
        source,
        git: true,
    }
}

/// The project's files as one commit's `FilesChanged`.
async fn files(project: &Project) -> anyhow::Result<Vec<Event>> {
    let mut work = Workspace::new(&project.dir);
    let scratch = tempdir()?;
    let repo = Repo::open(&scratch).await?;
    let snapshot = repo
        .snapshot(&mut work, None, "imported")
        .await
        .context("committing the project's files")?;
    let _ = std::fs::remove_dir_all(&scratch);
    let events = snapshot
        .map(|s| dirigent_sync::events(&s))
        .unwrap_or_default();
    Ok(events)
}

fn tempdir() -> anyhow::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("dirigent-import-{}", uuid(rand::random(), 4)));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub async fn import(mut api: Api, ask: Import) -> anyhow::Result<()> {
    let path = match ask.agent {
        Agent::ClaudeCode => claude_code::find(ask.session.as_deref())?,
        Agent::Codex => codex::find(ask.session.as_deref())?,
    };
    let mut conv = match ask.agent {
        Agent::ClaudeCode => claude_code::read(&path)?,
        Agent::Codex => codex::read(&path)?,
    };
    anyhow::ensure!(
        !conv.events.is_empty(),
        "{} has no conversation to import",
        path.display()
    );
    let mut found = redact::Found::default();
    for (_, event) in &mut conv.events {
        if ask.redact {
            match event {
                Event::ToolResult { content, .. } => found.redact(content),
                Event::AssistantMessage { content, .. } => {
                    for block in content {
                        if let ContentBlock::ToolResult { content, .. } = block {
                            found.redact(content);
                        }
                    }
                }
                _ => {}
            }
        }
        strings(event, &mut clip);
    }

    let dir = ask.dir.clone().unwrap_or_else(|| conv.cwd.clone());
    let project = match dir.is_dir() {
        true => Some(project(&dir).await),
        false => None,
    };
    // outside git, only a project asked for: a session may have worked anywhere, $HOME too
    let project = project.filter(|p| p.git || ask.dir.is_some());
    let files = match &project {
        Some(p) => files(p).await?,
        None => Vec::new(),
    };

    let spec = SessionSpec {
        model: ask
            .model
            .clone()
            .or(conv.model.clone())
            .unwrap_or_else(|| SessionSpec::default().model),
        effort: ask.effort.or(SessionSpec::default().effort),
        source: project.as_ref().and_then(|p| p.source.clone()),
        provider: ask.provider.unwrap_or(ask.agent.provider()),
        ..SessionSpec::default()
    };

    let turns = conv
        .events
        .iter()
        .filter(|(_, e)| matches!(e, Event::TurnStarted { .. }))
        .count();
    let bytes: usize = conv
        .events
        .iter()
        .map(|(_, e)| serde_json::to_string(e).map_or(0, |s| s.len()))
        .sum();
    println!("{}: {}", ask.agent.name(), path.display());
    println!(
        "  {} events, {turns} turns, {:.1} MB, {} to {}",
        conv.events.len(),
        bytes as f64 / 1e6,
        conv.first,
        conv.last
    );
    if let Some((title, _)) = &conv.title {
        println!("  title: {title}");
    }
    println!("  model: {}, provider: {:?}", spec.model, spec.provider);
    match (&project, &spec.source) {
        (Some(p), Some(Source::Git { url, reference })) => println!(
            "  workspace: {url}{}, with the files of {}",
            reference
                .as_ref()
                .map_or(String::new(), |r| format!(" ({r})")),
            p.dir.display()
        ),
        (Some(p), None) => println!(
            "  workspace: the files of {} (no git remote)",
            p.dir.display()
        ),
        (None, _) => println!(
            "  workspace: empty ({} isn't a git repository here; --dir gives the project)",
            dir.display()
        ),
    }
    for (what, n) in found.counts() {
        println!("  redacted in the conversation: {n} × {what}");
    }
    if ask.dry_run {
        return Ok(());
    }

    #[derive(Deserialize)]
    struct Me {
        subject: String,
    }
    let me: Me = api.get("/me").await?;
    let id = match ask.again {
        true => uuid(rand::random(), 4),
        false => session_id(&me.subject, ask.agent, &conv.session),
    };
    let made = api
        .put_status(&format!("/sessions/{id}"), &spec)
        .await
        .context("making the session")?;
    if made == reqwest::StatusCode::OK {
        anyhow::bail!("already imported as {id} (--again imports it into a new session)");
    }
    let written = write(&mut api, &id, &conv, &files, ask.agent, ask.chlive).await;
    // the session goes to players either way: one that's half written is deleted
    let done = api.delete(&format!("/sessions/{id}/import")).await;
    if let Err(e) = written {
        let _ = api.post_empty(&format!("/sessions/{id}/archive")).await;
        let _ = api.delete(&format!("/sessions/{id}")).await;
        return Err(e.context("the import failed, and its session is deleted: try again"));
    }
    done.context("finishing the import")?;
    println!("Imported as {id}");
    Ok(())
}

/// The conversation, its title, the project's files and the import itself, into the session's
/// log as its importer, in order.
async fn write(
    api: &mut Api,
    id: &str,
    conv: &Conversation,
    files: &[Event],
    agent: Agent,
    chlive: Option<String>,
) -> anyhow::Result<()> {
    #[derive(Deserialize)]
    struct Token {
        token: String,
    }
    let path = format!("/sessions/{id}/import");
    let Token { token } = api.post(&path, &()).await?;
    let url = match chlive {
        Some(url) => url,
        None => crate::sync::live(&api.creds.api)?,
    };
    // asked again, another token for the same importer
    let api = Arc::new(Mutex::new(api.clone()));
    let first = Arc::new(Mutex::new(Some(token)));
    let credentials = move || {
        let (api, first, path) = (api.clone(), first.clone(), path.clone());
        async move {
            if let Some(token) = first.lock().await.take() {
                return Ok(token);
            }
            let Token { token } = api.lock().await.post(&path, &()).await?;
            Ok::<_, anyhow::Error>(token)
        }
    };
    let client = LiveClient::new(ClientOptions::new(&url).credentials(credentials));

    let mut rows: Vec<EventRow> = conv
        .events
        .iter()
        .map(|(eid, e)| EventRow::new(id, eid.clone(), e))
        .collect();
    let new = || uuid(rand::random(), 4);
    if let Some((title, hold)) = &conv.title {
        let titled = Event::Titled {
            title: title.clone(),
            description: String::new(),
            hold: *hold,
        };
        rows.push(EventRow::new(id, new(), &titled));
    }
    rows.extend(files.iter().map(|e| EventRow::new(id, new(), e)));
    let imported = Event::Imported {
        from: agent.name().into(),
        session: conv.session.clone(),
        cwd: conv.cwd.display().to_string(),
        first: conv.first.clone(),
        last: conv.last.clone(),
    };
    rows.push(EventRow::new(id, new(), &imported));

    let total = rows.len();
    let mut last: Option<(i32, i64)> = None;
    let mut sent = 0;
    let mut rest = &rows[..];
    while !rest.is_empty() {
        let mut n = 0;
        let mut bytes = 0;
        while n < rest.len() && n < MAX_ROWS {
            let size = rest[n].data.len() + 200;
            if n > 0 && bytes + size > MAX_BYTES {
                break;
            }
            bytes += size;
            n += 1;
        }
        let ack = client
            .insert(EVENTS, &rest[..n])
            .await
            .map_err(|e| anyhow::anyhow!("writing the log: {e}"))?;
        // one partition, in order: the log reads as it was written
        for offset in ack.offsets {
            anyhow::ensure!(
                last.is_none_or(|l| l.0 == offset.0 && l.1 < offset.1),
                "the log took the rows out of order"
            );
            last = Some(offset);
        }
        rest = &rest[n..];
        sent += n;
        eprint!("\r  written {sent}/{total}");
    }
    eprintln!();
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A log whose ids are `e0`, `e1`, … in order.
    pub fn log() -> Log {
        let mut n = 0;
        Log::with_ids(Box::new(move || {
            n += 1;
            format!("e{}", n - 1)
        }))
    }

    /// The events' kinds, and their texts where they have one, to compare in tests.
    pub fn kinds(events: &[(String, Event)]) -> Vec<String> {
        events
            .iter()
            .map(|(_, e)| match e {
                Event::UserMessage { text, .. } => format!("user:{text}"),
                Event::TurnStarted { input } => format!("turn:{input}"),
                Event::Steered { input } => format!("steer:{input}"),
                Event::AssistantMessage {
                    content,
                    stop_reason,
                    ..
                } => {
                    let blocks: Vec<String> = content
                        .iter()
                        .map(|b| match b {
                            ContentBlock::Text { text } => format!("text:{text}"),
                            ContentBlock::Thinking { text, .. } => format!("thinking:{text}"),
                            ContentBlock::ToolCall { id, name, .. } => format!("call:{name}:{id}"),
                            ContentBlock::ToolResult { call_id, .. } => format!("result:{call_id}"),
                        })
                        .collect();
                    format!("reply:{stop_reason:?}:[{}]", blocks.join(","))
                }
                Event::ToolResult {
                    call_id,
                    name,
                    content,
                    is_error,
                    ..
                } => format!("result:{name}:{call_id}:{is_error}:{content}"),
                Event::TurnEnded { reason, .. } => format!("end:{reason:?}"),
                Event::Compacted {
                    summary,
                    first_kept,
                    ..
                } => format!("compacted:{first_kept}:{summary}"),
                other => other.kind(),
            })
            .collect()
    }

    #[test]
    fn a_uuid_has_its_version_and_variant() {
        let id = uuid([0xff; 16], 4);
        assert_eq!(id, "ffffffff-ffff-4fff-bfff-ffffffffffff");
        let a = session_id("user:a", Agent::ClaudeCode, "s");
        assert_eq!(a, session_id("user:a", Agent::ClaudeCode, "s"));
        assert_ne!(a, session_id("user:b", Agent::ClaudeCode, "s"));
        assert_ne!(a, session_id("user:a", Agent::Codex, "s"));
        assert_eq!(&a[14..15], "8");
    }

    #[test]
    fn inputs_start_turns_or_join_the_open_one() {
        let mut log = log();
        log.prompt("a".into());
        log.input("note".into());
        log.prompt("b".into());
        log.end(TurnEndReason::Aborted);
        log.input("c".into());
        log.compacted("S".into(), None);
        log.finish();
        assert_eq!(
            kinds(&log.events),
            [
                "user:a",
                "turn:e0",
                "user:note",
                "steer:e2",
                "end:Completed",
                "user:b",
                "turn:e5",
                "end:Aborted",
                "user:c",
                "turn:e8",
                "compacted:e10:S",
                "end:Aborted",
            ]
        );
    }

    #[test]
    fn a_long_string_is_clipped_in_its_middle() {
        let mut s = format!("{}{}", "a".repeat(MAX_STRING), "é".repeat(MAX_STRING));
        clip(&mut s);
        assert!(s.len() < MAX_STRING + 100, "{}", s.len());
        assert!(s.starts_with("aaa") && s.ends_with("éé"));
        assert!(s.contains("characters left out on import"));
        let mut short = "short".to_string();
        clip(&mut short);
        assert_eq!(short, "short");
    }
}
