//! The dirigent's part: tools that direct other sessions, and the follower that tells it what
//! happens in them. It acts as the session's owner (delegation): through control's HTTP API
//! with the owner as the caller, and through chlive as the owner, so it reaches what they do
//! (their sessions, and those they view) and writes what they may (inputs, into their own).
//! Its messages say `via` this session.
//!
//! The sessions it follows, and from where, are its own log's: each tool result that touched a
//! session holds `{"directs": {"session", "offset"}}` in its meta (the offset of what it wrote
//! there), and each [`Event::SessionUpdate`] it was sent moves the session's mark on. A turn
//! ending past the mark, or the session's player failing or detaching, comes as a new update,
//! written into this session by the follower (as its player): an input, as a user's message is.
//! A new player picks up where the last left off, since both come from the log.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use async_trait::async_trait;
use chlive_client::{ClientOptions, LiveClient};
use dirigent_protocol::control::{pb, Effort, SessionInfo};
use dirigent_protocol::fold::Fold;
use dirigent_protocol::rows::{EventRow, EVENTS, LOG_QUERY};
use dirigent_protocol::{ContentBlock, Event, Update};
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::provider::ToolSpec;
use crate::session::control::{owner_token, player_token};
use crate::tools::{
    opt_bool, opt_str, opt_u64, req_str, truncate_middle, Tool, ToolCtx, ToolOutput,
};

/// How long chlive may take to let the owner into a session just made (its grant, over CDC).
const GRANT_WAIT: Duration = Duration::from_secs(30);
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// A session's last reply, as an update or a read gives it.
const MAX_REPLY: usize = 4000;
const MAX_READ: usize = 30_000;
/// A session's events the follower reads.
const FOLLOW_QUERY: &str = "select session_id, offset, partition, ts, id, author, kind, data \
     from session_events where session_id = {session:UUID} \
     and kind in ('assistant_message', 'turn_ended', 'player_failed', 'player_detached') \
     order by offset";

/// What the dirigent acts with: as its owner, on behalf of its session `me`, with the
/// credentials its lease gives it for that.
pub struct Director {
    me: String,
    tokens: watch::Receiver<pb::Tokens>,
    /// chlive, as the owner.
    chlive: LiveClient,
    /// Control's HTTP API.
    control: String,
    http: reqwest::Client,
}

impl Director {
    pub fn new(
        me: &str,
        tokens: watch::Receiver<pb::Tokens>,
        chlive_url: &str,
        control: &str,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            tokens.borrow().owner.is_some(),
            "the lease has no credentials to act as the session's owner"
        );
        let opts = ClientOptions::new(chlive_url).credentials(owner_token(tokens.clone()));
        Ok(Director {
            me: me.to_string(),
            tokens,
            chlive: LiveClient::new(opts),
            control: control.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder().timeout(HTTP_TIMEOUT).build()?,
        })
    }

    /// The tools, for the agent's registry.
    pub fn tools(self: &Arc<Self>) -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(ListSessions(self.clone())),
            Arc::new(CreateSession(self.clone())),
            Arc::new(SendMessage(self.clone())),
            Arc::new(ReadSession(self.clone())),
            Arc::new(InterruptSession(self.clone())),
            Arc::new(StartSession(self.clone())),
            Arc::new(StopSession(self.clone())),
        ]
    }

    /// A call to control's API, as the owner; an error says what control answered.
    async fn api(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<reqwest::Response, String> {
        let token = self.tokens.borrow().owner.clone().unwrap_or_default();
        let mut req = self
            .http
            .request(method, format!("{}{path}", self.control))
            .bearer_auth(token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await.map_err(|e| format!("control: {e}"))?;
        if res.status().is_success() {
            return Ok(res);
        }
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        Err(format!("control answered {status}: {text}"))
    }

    async fn sessions(&self) -> Result<Vec<SessionInfo>, String> {
        let res = self.api(reqwest::Method::GET, "/sessions", None).await?;
        res.json()
            .await
            .map_err(|e| format!("control's sessions: {e}"))
    }

    /// Write `event` into `session`, as the owner, waiting up to `wait` for chlive to let them
    /// in; the row's offset there.
    async fn write(&self, session: &str, event: &Event, wait: Duration) -> Result<i64, String> {
        let row = EventRow::new(session, uuid::Uuid::new_v4().to_string(), event);
        let started = Instant::now();
        loop {
            match self.chlive.insert(EVENTS, [&row]).await {
                Ok(ack) => return Ok(ack.offsets.first().map_or(0, |(_, o)| *o)),
                Err(_) if started.elapsed() < wait => {
                    tokio::time::sleep(Duration::from_millis(200)).await
                }
                Err(e) => return Err(format!("writing into the session: {}", e.message)),
            }
        }
    }

    /// The log of `session` as it is now.
    async fn log(&self, session: &str) -> Result<Vec<EventRow>, String> {
        let rows = self
            .chlive
            .query(LOG_QUERY)
            .param("session", session)
            .complete()
            .fetch::<EventRow>()
            .await
            .map_err(|e| format!("reading the session: {}", e.message))?;
        Ok(rows.iter().map(|r| r.as_ref().clone()).collect())
    }

    fn message(&self, text: &str) -> Event {
        Event::UserMessage {
            text: text.to_string(),
            via: Some(self.me.clone()),
        }
    }
}

/// A tool result's meta, saying the dirigent touched `session`, writing at `offset`.
fn directs(session: &str, offset: i64) -> Value {
    json!({ "directs": { "session": session, "offset": offset } })
}

fn session_id(input: &Value) -> Result<String, String> {
    let s = req_str(input, "session")?;
    uuid::Uuid::parse_str(s)
        .map(|u| u.to_string())
        .map_err(|_| format!("{s:?} isn't a session id"))
}

fn state(s: &SessionInfo) -> &'static str {
    match &s.player {
        None => "idle",
        Some(h) if h.attached => "active",
        Some(_) => "away",
    }
}

fn spec(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        }),
    }
}

const SESSION: &str = "The session's id (a UUID).";

struct ListSessions(Arc<Director>);

#[async_trait]
impl Tool for ListSessions {
    fn spec(&self) -> ToolSpec {
        spec(
            "list_sessions",
            "List the sessions you can reach: each one's id, part (coder or dirigent), state (active: a player runs it; away: its player dropped and may come back; idle: none), model, source repository and ref, and your role in it (owner: you may write to it, start and stop it; viewer: you may only read it). Optionally only those whose repository URL contains `repo`.",
            json!({ "repo": { "type": "string", "description": "A part of the repository URL to match." } }),
            &[],
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, _: &ToolCtx, _: CancellationToken) -> ToolOutput {
        let d = &self.0;
        let all = match d.sessions().await {
            Ok(all) => all,
            Err(e) => return ToolOutput::error(e),
        };
        let repo = opt_str(&input, "repo");
        let lines: Vec<String> = all
            .iter()
            .filter(|s| s.id != d.me)
            .filter_map(|s| {
                let source = match &s.spec.source {
                    Some(dirigent_protocol::control::Source::Git { url, reference }) => {
                        match reference {
                            Some(r) => format!("{url}@{r}"),
                            None => url.clone(),
                        }
                    }
                    None => "-".into(),
                };
                if repo.is_some_and(|r| !source.contains(r)) {
                    return None;
                }
                let part = format!("{:?}", s.spec.part).to_lowercase();
                let role = format!("{:?}", s.role).to_lowercase();
                Some(format!(
                    "{}\t{part}\t{}\t{}\t{source}\t{role}",
                    s.id,
                    state(s),
                    s.spec.model
                ))
            })
            .collect();
        if lines.is_empty() {
            return ToolOutput::ok("No sessions.");
        }
        ToolOutput::ok(format!(
            "id\tpart\tstate\tmodel\tsource\trole\n{}",
            lines.join("\n")
        ))
    }
}

struct CreateSession(Arc<Director>);

#[async_trait]
impl Tool for CreateSession {
    fn spec(&self) -> ToolSpec {
        spec(
            "create_session",
            "Create a coding session, start a player for it, and send it its first message (its task). Its workspace is a clone of the git repository `git` at `ref` (by default its default branch), or empty without one. You're told when its turns end. Returns its id.",
            json!({
                "git": { "type": "string", "description": "The repository's URL (public)." },
                "ref": { "type": "string", "description": "A branch or tag." },
                "model": { "type": "string", "description": "The model; by default the default one." },
                "effort": { "type": "string", "enum": ["low", "medium", "high", "max", "none"], "description": "How hard it thinks; none: no extended thinking." },
                "message": { "type": "string", "description": "Its task, as its first message." }
            }),
            &["message"],
        )
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn call(&self, input: Value, _: &ToolCtx, _: CancellationToken) -> ToolOutput {
        let d = &self.0;
        let message = match req_str(&input, "message") {
            Ok(m) => m,
            Err(e) => return ToolOutput::error(e),
        };
        let mut body = json!({ "start": true });
        if let Some(git) = opt_str(&input, "git") {
            body["source"] = json!({ "git": { "url": git } });
            if let Some(r) = opt_str(&input, "ref") {
                body["source"]["git"]["ref"] = json!(r);
            }
        }
        if let Some(m) = opt_str(&input, "model") {
            body["model"] = json!(m);
        }
        match opt_str(&input, "effort") {
            Some("none") => body["effort"] = Value::Null,
            Some(e) => match serde_json::from_value::<Effort>(json!(e)) {
                Ok(_) => body["effort"] = json!(e),
                Err(_) => return ToolOutput::error(format!("{e:?} isn't an effort")),
            },
            None => {}
        }
        let id = uuid::Uuid::new_v4().to_string();
        let path = format!("/sessions/{id}");
        if let Err(e) = d.api(reqwest::Method::PUT, &path, Some(body)).await {
            return ToolOutput::error(e);
        }
        info!(session = id, "session created");
        match d.write(&id, &d.message(message), GRANT_WAIT).await {
            Ok(offset) => ToolOutput::ok(format!("Created and started the session {id}."))
                .with_meta(directs(&id, offset)),
            Err(e) => ToolOutput::error(format!("Created the session {id}, but {e}")),
        }
    }
}

struct SendMessage(Arc<Director>);

#[async_trait]
impl Tool for SendMessage {
    fn spec(&self) -> ToolSpec {
        spec(
            "send_message",
            "Send a message to a session you own, starting it if it's idle. A session that's working takes it into its running turn; otherwise it starts a turn on it. You're told when its turns end.",
            json!({
                "session": { "type": "string", "description": SESSION },
                "text": { "type": "string", "description": "The message." }
            }),
            &["session", "text"],
        )
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn call(&self, input: Value, _: &ToolCtx, _: CancellationToken) -> ToolOutput {
        let d = &self.0;
        let (session, text) = match (session_id(&input), req_str(&input, "text")) {
            (Ok(s), Ok(t)) => (s, t),
            (Err(e), _) | (_, Err(e)) => return ToolOutput::error(e),
        };
        let start = format!("/sessions/{session}/start");
        if let Err(e) = d.api(reqwest::Method::POST, &start, None).await {
            return ToolOutput::error(e);
        }
        match d.write(&session, &d.message(text), Duration::ZERO).await {
            Ok(offset) => ToolOutput::ok("Sent.").with_meta(directs(&session, offset)),
            Err(e) => ToolOutput::error(e),
        }
    }
}

struct InterruptSession(Arc<Director>);

#[async_trait]
impl Tool for InterruptSession {
    fn spec(&self) -> ToolSpec {
        spec(
            "interrupt_session",
            "Interrupt the turn a session you own is running: its model call and tools are cancelled, and the turn ends.",
            json!({ "session": { "type": "string", "description": SESSION } }),
            &["session"],
        )
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn call(&self, input: Value, _: &ToolCtx, _: CancellationToken) -> ToolOutput {
        let d = &self.0;
        let session = match session_id(&input) {
            Ok(s) => s,
            Err(e) => return ToolOutput::error(e),
        };
        let log = match d.log(&session).await {
            Ok(log) => log,
            Err(e) => return ToolOutput::error(e),
        };
        let mut fold = Fold::default();
        for row in &log {
            if let Ok(ev) = row.event() {
                fold.apply(&row.id, &ev);
            }
        }
        let Some(turn) = fold.open().map(|t| t.id.clone()) else {
            return ToolOutput::error("The session isn't running a turn.");
        };
        match d
            .write(&session, &Event::UserInterrupt { turn }, Duration::ZERO)
            .await
        {
            Ok(offset) => ToolOutput::ok("Interrupted.").with_meta(directs(&session, offset)),
            Err(e) => ToolOutput::error(e),
        }
    }
}

struct StartSession(Arc<Director>);

#[async_trait]
impl Tool for StartSession {
    fn spec(&self) -> ToolSpec {
        spec(
            "start_session",
            "Start a session you own that's idle: a player for it picks up its log where it was.",
            json!({ "session": { "type": "string", "description": SESSION } }),
            &["session"],
        )
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn call(&self, input: Value, _: &ToolCtx, _: CancellationToken) -> ToolOutput {
        let session = match session_id(&input) {
            Ok(s) => s,
            Err(e) => return ToolOutput::error(e),
        };
        let path = format!("/sessions/{session}/start");
        match self.0.api(reqwest::Method::POST, &path, None).await {
            Ok(_) => ToolOutput::ok("Started."),
            Err(e) => ToolOutput::error(e),
        }
    }
}

struct StopSession(Arc<Director>);

#[async_trait]
impl Tool for StopSession {
    fn spec(&self) -> ToolSpec {
        spec(
            "stop_session",
            "Stop a session you own: its player finishes the running turn (with `now`, cancels it) and lets the session go idle. Its log and workspace stay; starting it again picks up from there.",
            json!({
                "session": { "type": "string", "description": SESSION },
                "now": { "type": "boolean", "description": "Cancel the running turn instead of letting it finish." }
            }),
            &["session"],
        )
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn call(&self, input: Value, _: &ToolCtx, _: CancellationToken) -> ToolOutput {
        let session = match session_id(&input) {
            Ok(s) => s,
            Err(e) => return ToolOutput::error(e),
        };
        let now = opt_bool(&input, "now").unwrap_or(false);
        let path = format!("/sessions/{session}/stop");
        let body = Some(json!({ "now": now }));
        match self.0.api(reqwest::Method::POST, &path, body).await {
            Ok(_) => ToolOutput::ok("Stopping."),
            Err(e) => ToolOutput::error(e),
        }
    }
}

struct ReadSession(Arc<Director>);

#[async_trait]
impl Tool for ReadSession {
    fn spec(&self) -> ToolSpec {
        spec(
            "read_session",
            "Read what happened in a session, condensed: its workspace, and for each of its last turns the input, the tools it ran, its last reply and how the turn ended (or that it's still running).",
            json!({
                "session": { "type": "string", "description": SESSION },
                "turns": { "type": "integer", "description": "How many of its last turns (default 5)." }
            }),
            &["session"],
        )
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, _: &ToolCtx, _: CancellationToken) -> ToolOutput {
        let session = match session_id(&input) {
            Ok(s) => s,
            Err(e) => return ToolOutput::error(e),
        };
        let turns = opt_u64(&input, "turns").unwrap_or(5).max(1) as usize;
        match self.0.log(&session).await {
            Ok(log) => {
                let (text, _) = truncate_middle(&condense(&log, turns), MAX_READ, MAX_READ / 2);
                ToolOutput::ok(text)
            }
            Err(e) => ToolOutput::error(e),
        }
    }
}

/// One turn of a session, as `read_session` tells it.
#[derive(Default)]
struct TurnSummary {
    inputs: Vec<String>,
    /// Each tool it ran, how often, and how often it failed.
    tools: Vec<(String, usize, usize)>,
    reply: String,
    ended: Option<String>,
}

/// A session's log, condensed: its workspace, its player's comings and goings, and its last
/// `turns` turns.
fn condense(log: &[EventRow], turns: usize) -> String {
    let mut fold = Fold::default();
    let mut notes: Vec<String> = Vec::new();
    let mut all: Vec<TurnSummary> = Vec::new();
    let mut queued: HashMap<String, String> = HashMap::new();
    for row in log {
        let Ok(ev) = row.event() else { continue };
        if let Some(text) = ev.input() {
            queued.insert(row.id.clone(), text);
        }
        fold.apply(&row.id, &ev);
        match ev {
            Event::TurnStarted { input } => all.push(TurnSummary {
                inputs: queued.remove(&input).into_iter().collect(),
                ..Default::default()
            }),
            Event::Steered { input } => {
                if let (Some(t), Some(text)) = (all.last_mut(), queued.remove(&input)) {
                    t.inputs.push(text);
                }
            }
            Event::AssistantMessage { content, .. } => {
                if let Some(t) = all.last_mut() {
                    let text: String = content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect();
                    if !text.trim().is_empty() {
                        t.reply = text;
                    }
                }
            }
            Event::ToolResult { name, is_error, .. } => {
                if let Some(t) = all.last_mut() {
                    match t.tools.iter_mut().find(|(n, ..)| *n == name) {
                        Some((_, n, failed)) => {
                            *n += 1;
                            *failed += is_error as usize;
                        }
                        None => t.tools.push((name, 1, is_error as usize)),
                    }
                }
            }
            Event::TurnEnded { reason, interrupt } => {
                if let Some(t) = all.last_mut() {
                    let why = match interrupt {
                        Some(_) => "interrupted".to_string(),
                        None => format!("{reason:?}").to_lowercase(),
                    };
                    t.ended = Some(why);
                }
            }
            Event::WorkspaceReady { source, head, .. } => notes.push(format!(
                "workspace: {}{}",
                match source {
                    Some(dirigent_protocol::control::Source::Git { url, reference }) => format!(
                        "{url}{}",
                        reference.map(|r| format!("@{r}")).unwrap_or_default()
                    ),
                    None => "empty".into(),
                },
                head.map(|h| format!(" at {h}")).unwrap_or_default()
            )),
            Event::PlayerFailed { error, .. } => notes.push(format!("player failed: {error}")),
            Event::CheckpointFailed { error, .. } => {
                notes.push(format!("checkpoint failed: {error}"))
            }
            _ => {}
        }
    }
    let mut out = String::new();
    notes.dedup();
    for n in notes.iter().rev().take(3).rev() {
        out.push_str(&format!("{n}\n"));
    }
    let skipped = all.len().saturating_sub(turns);
    if skipped > 0 {
        out.push_str(&format!("({skipped} earlier turns)\n"));
    }
    for (i, t) in all.iter().enumerate().skip(skipped) {
        out.push_str(&format!("\n## Turn {}\n", i + 1));
        for input in &t.inputs {
            out.push_str(&format!("Input: {input}\n"));
        }
        if !t.tools.is_empty() {
            let tools: Vec<String> = t
                .tools
                .iter()
                .map(|(n, k, f)| match f {
                    0 => format!("{n}×{k}"),
                    f => format!("{n}×{k} ({f} failed)"),
                })
                .collect();
            out.push_str(&format!("Tools: {}\n", tools.join(", ")));
        }
        if !t.reply.is_empty() {
            let (reply, _) = truncate_middle(&t.reply, MAX_REPLY, MAX_REPLY / 2);
            out.push_str(&format!("Reply: {reply}\n"));
        }
        match &t.ended {
            Some(why) => out.push_str(&format!("Ended: {why}\n")),
            None => out.push_str("Still running.\n"),
        }
    }
    let waiting = fold.queued().len();
    if waiting > 0 {
        out.push_str(&format!("\n{waiting} message(s) waiting for a turn.\n"));
    }
    if out.is_empty() {
        out.push_str("Nothing has happened in it yet.");
    }
    out
}

/// Follows the sessions this one directs (see the module's doc), writing their updates into
/// it as its player, until its log ends.
pub async fn follow(director: Arc<Director>, chlive_url: String) {
    let opts = ClientOptions::new(&chlive_url).credentials(player_token(director.tokens.clone()));
    let me = LiveClient::new(opts);
    let (rows, mut live) = match me
        .query(LOG_QUERY)
        .param("session", &director.me)
        .history::<EventRow>()
        .await
    {
        Ok(log) => log,
        Err(e) => {
            warn!("the follower can't read the session's log: {}", e.message);
            return;
        }
    };
    let mut marks: HashMap<String, watch::Sender<i64>> = HashMap::new();
    let note = |row: &EventRow, marks: &mut HashMap<String, watch::Sender<i64>>| {
        let Some((session, offset)) = mark(row) else {
            return;
        };
        match marks.get(&session) {
            Some(m) => {
                m.send_if_modified(|v| {
                    let raised = offset > *v;
                    *v = (*v).max(offset);
                    raised
                });
            }
            None => {
                let (tx, rx) = watch::channel(offset);
                marks.insert(session.clone(), tx);
                tokio::spawn(follow_one(director.clone(), me.clone(), session, rx));
            }
        }
    };
    for row in rows.iter() {
        note(row, &mut marks);
    }
    while let Some(row) = live.next().await {
        match row {
            Ok(row) => note(&row, &mut marks),
            Err(e) => {
                warn!("the follower's log subscription failed: {}", e.message);
                return;
            }
        }
    }
}

/// The session a row of this one's log marks, and its offset there.
fn mark(row: &EventRow) -> Option<(String, i64)> {
    match row.event().ok()? {
        Event::ToolResult { meta, .. } => {
            let d = meta.get("directs")?;
            Some((
                d.get("session")?.as_str()?.to_string(),
                d.get("offset")?.as_i64()?,
            ))
        }
        Event::SessionUpdate {
            session, offset, ..
        } => Some((session, offset as i64)),
        _ => None,
    }
}

/// Follows `session`, writing an update into this one for each of its turn ends, and its
/// player failing or detaching, past the mark.
async fn follow_one(
    d: Arc<Director>,
    me: LiveClient,
    session: String,
    mut mark: watch::Receiver<i64>,
) {
    info!(session, "following");
    let sub = d
        .chlive
        .query(FOLLOW_QUERY)
        .param("session", &session)
        .history::<EventRow>()
        .await;
    let (rows, mut live) = match sub {
        Ok(s) => s,
        Err(e) => {
            warn!(session, "can't follow: {}", e.message);
            return;
        }
    };
    let mut reply: Option<String> = None;
    let mut seen = |row: &EventRow| -> Option<Event> {
        let ev = row.event().ok()?;
        let update = match ev {
            Event::AssistantMessage { content, .. } => {
                let text: String = content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                if !text.trim().is_empty() {
                    reply = Some(truncate_middle(&text, MAX_REPLY, MAX_REPLY / 2).0);
                }
                return None;
            }
            Event::TurnEnded { reason, .. } => Update::TurnEnded { reason },
            Event::PlayerFailed { error, .. } => Update::PlayerFailed { error },
            Event::PlayerDetached { .. } => Update::PlayerDetached,
            _ => return None,
        };
        if row.offset as i64 <= *mark.borrow_and_update() {
            return None;
        }
        Some(Event::SessionUpdate {
            session: session.clone(),
            offset: row.offset,
            update,
            reply: reply.take(),
        })
    };
    let mut pending: Vec<Event> = rows.iter().filter_map(|r| seen(r)).collect();
    loop {
        for update in pending.drain(..) {
            let row = EventRow::new(&d.me, uuid::Uuid::new_v4().to_string(), &update);
            if let Err(e) = me.insert(EVENTS, [&row]).await.context("writing an update") {
                warn!("{e:#}");
            }
        }
        match live.next().await {
            Some(Ok(row)) => pending.extend(seen(&row)),
            Some(Err(e)) => {
                warn!(session, "following failed: {}", e.message);
                return;
            }
            None => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dirigent_protocol::{StopReason, TurnEndReason, Usage};

    fn row(offset: u64, id: &str, ev: &Event) -> EventRow {
        let mut r = EventRow::new("s", id.into(), ev);
        r.offset = offset;
        r
    }

    #[test]
    fn a_log_condenses_to_its_turns() {
        let msg = |t: &str| Event::UserMessage {
            text: t.into(),
            via: None,
        };
        let reply = |t: &str, stop| Event::AssistantMessage {
            content: vec![ContentBlock::Text { text: t.into() }],
            stop_reason: stop,
            usage: Usage::default(),
        };
        let tool = |ok: bool| Event::ToolResult {
            call_id: "c".into(),
            name: "Bash".into(),
            content: "x".into(),
            is_error: !ok,
            meta: Value::Null,
        };
        let log = [
            row(
                1,
                "w",
                &Event::WorkspaceReady {
                    source: None,
                    head: None,
                    recreated: false,
                },
            ),
            row(2, "m1", &msg("fix it")),
            row(3, "t1", &Event::TurnStarted { input: "m1".into() }),
            row(4, "a1", &reply("looking", StopReason::ToolUse)),
            row(5, "r1", &tool(true)),
            row(6, "r2", &tool(false)),
            row(7, "a2", &reply("fixed", StopReason::EndTurn)),
            row(
                8,
                "e1",
                &Event::TurnEnded {
                    reason: TurnEndReason::Completed,
                    interrupt: None,
                },
            ),
            row(9, "m2", &msg("more")),
            row(10, "t2", &Event::TurnStarted { input: "m2".into() }),
            row(11, "m3", &msg("later")),
        ];
        let text = condense(&log, 5);
        assert_eq!(
            text,
            "workspace: empty\n\n## Turn 1\nInput: fix it\nTools: Bash×2 (1 failed)\nReply: fixed\nEnded: completed\n\n## Turn 2\nInput: more\nStill running.\n\n1 message(s) waiting for a turn.\n"
        );
        assert!(condense(&log, 1).contains("(1 earlier turns)\n\n## Turn 2"));
    }

    #[test]
    fn tool_results_and_updates_mark_the_sessions_followed() {
        let result = Event::ToolResult {
            call_id: "c".into(),
            name: "send_message".into(),
            content: "Sent.".into(),
            is_error: false,
            meta: directs("s1", 7),
        };
        assert_eq!(mark(&row(1, "a", &result)), Some(("s1".into(), 7)));
        let update = Event::SessionUpdate {
            session: "s1".into(),
            offset: 9,
            update: Update::PlayerDetached,
            reply: None,
        };
        assert_eq!(mark(&row(2, "b", &update)), Some(("s1".into(), 9)));
        let plain = Event::ToolResult {
            call_id: "c".into(),
            name: "list_sessions".into(),
            content: "".into(),
            is_error: false,
            meta: Value::Null,
        };
        assert_eq!(mark(&row(3, "c", &plain)), None);
    }
}
