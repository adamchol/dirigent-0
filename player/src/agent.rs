use std::collections::VecDeque;
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;
use tokio::sync::{mpsc, watch, Mutex};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::checkpoints::Checkpoints;
use crate::compact;
use crate::history::{History, CHARS_PER_TOKEN};
use crate::mirror::Mirror;
use crate::provider::{
    Accumulator, Completion, ContentBlock, Delta, Effort, Provider, ProviderError, Request,
    StopReason,
};
use crate::secrets::Secrets;
use crate::session::Session;
use crate::title;
use crate::tools::{Registry, ToolCtx, ToolOutput};
use dirigent_protocol::fold::Fold;
use dirigent_protocol::{Event, TurnEndReason};

#[derive(Debug, Clone, Copy)]
pub struct AgentConfig {
    pub model: &'static str,
    pub system: &'static str,
    pub effort: Option<Effort>,
    /// The model's context window, in tokens.
    pub context_window: u64,
    /// What names the session (see [`title`]): a small model, made to think little.
    pub title_model: &'static str,
    pub title_effort: Option<Effort>,
}

impl AgentConfig {
    /// A call whose context is estimated at this many tokens compacts it first.
    fn trigger(&self) -> u64 {
        self.context_window / 100 * 85
    }

    /// The tokens a compaction keeps as they are.
    fn keep(&self) -> u64 {
        (self.context_window / 10).min(20_000)
    }
}

/// Program-lifetime pieces; each session gets its own history and tool context.
pub struct Agent {
    pub provider: Arc<dyn Provider>,
    pub cfg: AgentConfig,
    pub tools: Arc<Registry>,
    pub workdir: &'static Path,
    /// Where the workspace is snapshotted after every turn; none, it isn't.
    pub checkpoints: Option<&'static Checkpoints>,
    /// Where the workspace is mirrored after every tool call that may change it; none, it isn't.
    pub mirror: Option<&'static Mutex<Mirror>>,
    /// The session's secrets: in its tools' environment, kept out of what's published.
    pub secrets: Secrets,
}

pub const ABORTED_TOOL_RESULT: &str = "Tool call aborted by user interrupt";

/// How long a title call may take; then the session goes on unnamed (until the next one).
const TITLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The word to leave the session (control's, through the lease): `when` becomes `Some(now)`
/// once it's stopped, and a graceful stop (`false`) may turn into one that's now.
pub struct Detach {
    /// The player's epoch, which `PlayerDetached` names.
    pub epoch: i64,
    pub when: watch::Receiver<Option<bool>>,
}

#[derive(Debug, Clone)]
struct Call {
    id: String,
    name: String,
    input: Value,
}

/// The turn in progress. `None` in the loop means idle.
struct Turn {
    /// Its `TurnStarted`'s id, which interrupts name.
    id: String,
    token: CancellationToken,
    /// Tool calls still waiting for a result.
    pending: Vec<String>,
    /// A user message arrived during the current model call.
    steered: bool,
    interrupted: bool,
    /// A compaction ran since the last reply, so another would gain nothing.
    compacted: bool,
    /// The user interrupt that stopped it, if one did.
    interrupt: Option<String>,
}

type Tx = mpsc::UnboundedSender<Event>;

/// A title call's outcome: the title and description, `None` if it made none.
type Named = Option<(String, String)>;

impl Agent {
    /// Run one session: user events come from the session, model and tool outcomes from the
    /// tasks this loop spawns. Every event the loop produces is published to the session; only
    /// what the model needs is kept in `history`. Returns when the session's events end and no
    /// turn is in progress.
    ///
    /// A message starts a turn when the loop is idle and steers the running one otherwise;
    /// messages the log leaves queued get a turn each. A turn the log leaves unfinished (the
    /// previous player stopped mid-turn) is closed first: its tool calls get aborted results and
    /// it ends Aborted.
    ///
    /// Told to detach, the loop starts no more turns (queued messages wait in the log for the
    /// next player) and, once the turn running ends (cancelled if the stop is now), records
    /// `PlayerDetached` and returns.
    pub async fn run(
        &self,
        session: &mut dyn Session,
        log: Vec<(String, Event)>,
        mut detach: Detach,
    ) -> anyhow::Result<()> {
        let mut fold = Fold::default();
        let handled: Vec<(String, Event)> = log
            .iter()
            .filter_map(|(id, ev)| Some((id.clone(), fold.apply(id, ev)?)))
            .collect();
        let tools = serde_json::to_string(self.tools.specs()).map_or(0, |s| s.len());
        let (tx, mut outcomes) = mpsc::unbounded_channel::<Event>();
        let (named, mut names) = mpsc::unbounded_channel::<Named>();
        let (title, hold) = match handled.iter().rev().find_map(|(_, ev)| match ev {
            Event::Titled {
                title,
                description,
                hold,
            } => Some((title.clone(), description.clone(), *hold)),
            _ => None,
        }) {
            Some((t, d, hold)) => (Some((t, d)), hold),
            None => (None, false),
        };
        let first = handled.iter().find_map(|(_, ev)| match ev {
            Event::UserMessage { text, .. } => Some(text.clone()),
            _ => None,
        });
        let mut run = Run {
            agent: self,
            session,
            history: History::from_events(&handled),
            fixed: (self.cfg.system.len() + tools) as u64 / CHARS_PER_TOKEN,
            ctx: Arc::new(ToolCtx::new(self.workdir, self.secrets.clone())),
            turn: None,
            queue: fold.queued().iter().cloned().collect(),
            tx,
            title,
            hold,
            titling: None,
            named,
        };
        // a session that went unnamed (its title call failed, or came before titles did)
        if let (None, Some(first)) = (&run.title, first) {
            run.name(&first);
        }
        let closing = close_unfinished(&handled, fold.open().and_then(|t| t.interrupt.clone()));
        if !closing.is_empty() {
            warn!("the log ends mid-turn; closing that turn");
        }
        for ev in &closing {
            run.record(ev).await;
        }
        info!(
            events = log.len(),
            queued = run.queue.len(),
            "session started"
        );
        let mut client = true;
        let mut secrets = self.secrets.clone();
        let mut seen = secrets.current();
        // `Some(now)` once told to detach (maybe before it started: no turn is begun then)
        let mut detaching: Option<bool> = *detach.when.borrow_and_update();
        if let Some(now) = detaching {
            info!(now, "detaching");
        }

        loop {
            if detaching.is_none() {
                run.next_turn().await;
            }
            if (!client || detaching.is_some()) && run.turn.is_none() && run.titling.is_none() {
                break;
            }
            tokio::select! {
                biased;
                Some(ev) = outcomes.recv() => run.handle(ev).await,
                Some(named) = names.recv() => run.named(named).await,
                Some(now) = secrets.changed(&seen) => {
                    let names: Vec<String> = now.keys().cloned().collect();
                    info!(?names, "the session's secrets changed");
                    seen = now;
                    run.session.publish(&Event::SecretsChanged { names }).await;
                }
                now = told_to_detach(&mut detach.when, detaching), if detaching != Some(true) => {
                    info!(now, "detaching");
                    detaching = Some(now);
                    if let (true, Some(t)) = (now, &mut run.turn) {
                        t.interrupted = true;
                        t.token.cancel();
                    }
                    if let (true, Some(naming)) = (now, &run.titling) {
                        naming.cancel();
                    }
                }
                ev = run.session.recv(), if client => match ev {
                    Some((id, ev)) => run.input(id, ev).await,
                    None => {
                        client = false;
                        run.queue.clear();
                        // Nobody is listening: stop the work and let it drain.
                        if let Some(t) = &mut run.turn {
                            t.interrupted = true;
                            t.token.cancel();
                        }
                    }
                },
            }
        }
        if detaching.is_some() {
            run.record(&Event::PlayerDetached {
                epoch: detach.epoch,
            })
            .await;
        }
        info!("session ended");
        Ok(())
    }
}

/// Once `when` says something other than `seen`: whether to stop now. Never, if its sender is
/// gone without a word.
async fn told_to_detach(when: &mut watch::Receiver<Option<bool>>, seen: Option<bool>) -> bool {
    loop {
        if let Some(now) = *when.borrow_and_update() {
            if Some(now) != seen {
                return now;
            }
        }
        if when.changed().await.is_err() {
            return std::future::pending().await;
        }
    }
}

/// The events that close a turn the handled `events` leave open: an aborted result for each
/// tool call still waiting for one, then `TurnEnded { Aborted }` naming `interrupt`, the
/// interrupt the log has for it. Empty when no turn is open.
fn close_unfinished(events: &[(String, Event)], interrupt: Option<String>) -> Vec<Event> {
    let mut open = false;
    let mut waiting: Vec<(String, String)> = Vec::new();
    for (_, ev) in events {
        match ev {
            Event::UserMessage { .. } => open = true,
            Event::AssistantMessage {
                content,
                stop_reason,
                ..
            } => {
                waiting = if *stop_reason == StopReason::ToolUse {
                    tool_calls(content)
                        .into_iter()
                        .map(|c| (c.id, c.name))
                        .collect()
                } else {
                    Vec::new()
                };
            }
            Event::ToolResult { call_id, .. } => waiting.retain(|(id, _)| id != call_id),
            Event::TurnEnded { .. } => {
                open = false;
                waiting.clear();
            }
            Event::PlayerAttached { .. }
            | Event::PlayerDetached { .. }
            | Event::WorkspaceReady { .. }
            | Event::PlayerFailed { .. }
            | Event::WorkspaceRestored { .. }
            | Event::Checkpoint { .. }
            | Event::CheckpointFailed { .. }
            | Event::UserInterrupt { .. }
            | Event::SessionUpdate { .. }
            | Event::TurnStarted { .. }
            | Event::Steered { .. }
            | Event::ModelDelta { .. }
            | Event::ModelCancelled
            | Event::Error { .. }
            | Event::Compacted { .. }
            | Event::CompactionFailed { .. }
            | Event::Titled { .. }
            | Event::FilesChanged { .. }
            | Event::SecretAnswered { .. }
            | Event::SecretsChanged { .. } => {}
        }
    }
    if !open {
        return Vec::new();
    }
    let mut out: Vec<Event> = waiting
        .into_iter()
        .map(|(call_id, name)| Event::ToolResult {
            call_id,
            name,
            content: ABORTED_TOOL_RESULT.into(),
            is_error: true,
            meta: Value::Null,
        })
        .collect();
    out.push(Event::TurnEnded {
        reason: TurnEndReason::Aborted,
        interrupt,
    });
    out
}

/// Per-session state of [`Agent::run`].
struct Run<'a> {
    agent: &'a Agent,
    session: &'a mut dyn Session,
    history: History,
    /// Estimated tokens of the system prompt and tools.
    fixed: u64,
    ctx: Arc<ToolCtx>,
    turn: Option<Turn>,
    /// Inputs waiting for a turn of their own: `(id, text)`.
    queue: VecDeque<(String, String)>,
    tx: Tx,
    /// The session's title and description, the log's last; none until it's named.
    title: Option<(String, String)>,
    /// Its owner named it and keeps it so: the player doesn't name it again.
    hold: bool,
    /// The title call running, if one is; the loop waits for it before it ends.
    titling: Option<CancellationToken>,
    named: mpsc::UnboundedSender<Named>,
}

impl Run<'_> {
    /// An event a user appended, with its id.
    async fn input(&mut self, id: String, ev: Event) {
        match ev {
            Event::UserMessage { .. }
            | Event::SessionUpdate { .. }
            | Event::SecretAnswered { .. } => {
                let text = ev.input().expect("an input");
                if self.turn.is_none() {
                    self.queue.push_back((id, text));
                    return;
                }
                info!("input (steers the turn)");
                let id = self.session.publish(&Event::Steered { input: id }).await;
                self.history
                    .push(&id, &Event::UserMessage { text, via: None });
                if let Some(t) = &mut self.turn {
                    t.steered = true;
                }
            }
            Event::UserInterrupt { turn } => match &mut self.turn {
                Some(t) if t.id == turn && t.interrupt.is_none() => {
                    info!("user interrupt");
                    t.interrupt = Some(id);
                    t.interrupted = true;
                    t.token.cancel();
                }
                _ => info!("ignoring an interrupt for a turn that isn't running"),
            },
            Event::Titled {
                title,
                description,
                hold,
            } => {
                info!(hold, "renamed: {title}");
                self.title = Some((title, description));
                self.hold = hold;
            }
            other => warn!(?other, "ignoring an event users don't send"),
        }
    }

    /// Start a turn on the next queued message, if idle.
    async fn next_turn(&mut self) {
        if self.turn.is_some() {
            return;
        }
        let Some((input, text)) = self.queue.pop_front() else {
            return;
        };
        info!("input (starts a turn)");
        let id = self.session.publish(&Event::TurnStarted { input }).await;
        if self.title.is_none() {
            self.name(&text);
        }
        self.history
            .push(&id, &Event::UserMessage { text, via: None });
        self.turn = Some(Turn {
            id,
            token: CancellationToken::new(),
            pending: Vec::new(),
            steered: false,
            interrupted: false,
            compacted: false,
            interrupt: None,
        });
        self.call_model();
    }

    /// An outcome of the model or tools tasks.
    async fn handle(&mut self, ev: Event) {
        match &ev {
            Event::ModelDelta { .. } => {
                if self.turn.is_some() {
                    self.session.publish(&ev).await;
                }
            }
            Event::AssistantMessage {
                content,
                stop_reason,
                ..
            } => {
                let Some(t) = &mut self.turn else { return };
                t.compacted = false;
                self.record(&ev).await;
                let calls = tool_calls(content);
                if *stop_reason == StopReason::ToolUse && !calls.is_empty() {
                    self.run_tools(calls);
                } else if self.turn.as_ref().is_some_and(|t| t.steered) {
                    self.call_model();
                } else {
                    self.end_turn(TurnEndReason::Completed).await;
                }
            }
            Event::ModelCancelled => {
                if self.turn.is_some() {
                    self.record(&ev).await;
                    self.end_turn(TurnEndReason::Aborted).await;
                }
            }
            Event::Error {
                retry_in_ms,
                compact,
                ..
            } => {
                if self.turn.is_some() {
                    self.record(&ev).await;
                    if *compact {
                        self.compact();
                    } else if retry_in_ms.is_none() {
                        self.end_turn(TurnEndReason::Failed).await;
                    }
                }
            }
            Event::Compacted { .. } | Event::CompactionFailed { .. } => {
                if self.turn.is_some() {
                    self.record(&ev).await;
                    if let (Event::Compacted { summary, .. }, false) = (&ev, self.hold) {
                        self.name(summary);
                    }
                    self.call_model();
                }
            }
            Event::ToolResult { call_id, .. } => {
                if self.turn.is_none() {
                    return;
                }
                self.record(&ev).await;
                let Some(t) = &mut self.turn else { return };
                t.pending.retain(|id| id != call_id);
                if !t.pending.is_empty() {
                    return;
                }
                if t.interrupted {
                    self.end_turn(TurnEndReason::Aborted).await;
                } else {
                    self.call_model();
                }
            }
            // Users' events and the loop's own decisions; never outcomes.
            Event::PlayerAttached { .. }
            | Event::PlayerDetached { .. }
            | Event::WorkspaceReady { .. }
            | Event::PlayerFailed { .. }
            | Event::WorkspaceRestored { .. }
            | Event::Checkpoint { .. }
            | Event::CheckpointFailed { .. }
            | Event::UserMessage { .. }
            | Event::UserInterrupt { .. }
            | Event::SessionUpdate { .. }
            | Event::TurnStarted { .. }
            | Event::Steered { .. }
            | Event::TurnEnded { .. }
            | Event::Titled { .. }
            | Event::SecretAnswered { .. }
            | Event::SecretsChanged { .. } => {}
            // the workspace, as a tool left it: in the log, never the history
            Event::FilesChanged { .. } => {
                self.session.publish(&ev).await;
            }
        }
    }

    /// Name the session after `input` (its first message, or a compaction's summary), in the
    /// background: the outcome comes to [`Run::named`]. Not while a title call runs already.
    fn name(&mut self, input: &str) {
        if self.titling.is_some() {
            return;
        }
        let token = CancellationToken::new();
        self.titling = Some(token.clone());
        let req = title::request(&self.agent.cfg, input, self.title.as_ref());
        let provider = self.agent.provider.clone();
        let named = self.named.clone();
        tokio::spawn(async move {
            let reply = tokio::time::timeout(
                TITLE_TIMEOUT,
                stream_reply(&*provider, &req, &token, &|_| {}),
            )
            .await;
            let _ = named.send(match reply {
                Ok(Ok(Some(c))) => {
                    let named = title::parse(&c);
                    if named.is_none() {
                        warn!("the title call made no title: {}", c.message.text());
                    }
                    named
                }
                Ok(Ok(None)) => None,
                Ok(Err(e)) => {
                    warn!("the title call failed: {e}");
                    None
                }
                Err(_) => {
                    warn!("the title call timed out");
                    None
                }
            });
        });
    }

    /// A title call's outcome: published, unless the owner holds a name of their own now, or
    /// it's the name the session has.
    async fn named(&mut self, named: Named) {
        self.titling = None;
        let Some(named) = named else { return };
        if self.hold || self.title.as_ref() == Some(&named) {
            return;
        }
        info!("titled: {}", named.0);
        let (title, description) = named.clone();
        self.session
            .publish(&Event::Titled {
                title,
                description,
                hold: false,
            })
            .await;
        self.title = Some(named);
    }

    /// Publish to the session and keep what the model needs.
    async fn record(&mut self, ev: &Event) {
        log_event(ev);
        let id = self.session.publish(ev).await;
        self.history.push(&id, ev);
    }

    /// The turn ends, and the workspace as it left it is checkpointed. The loop waits for the
    /// snapshot: what users send meanwhile waits in the session.
    async fn end_turn(&mut self, reason: TurnEndReason) {
        let (turn, interrupt) = match self.turn.take() {
            Some(t) => (Some(t.id), t.interrupt),
            None => (None, None),
        };
        self.record(&Event::TurnEnded { reason, interrupt }).await;
        let (Some(checkpoints), Some(turn)) = (self.agent.checkpoints, turn) else {
            return;
        };
        let ev = match checkpoints.snapshot(self.agent.workdir, &turn).await {
            Ok(snapshot) => Event::Checkpoint { turn, snapshot },
            Err(error) => Event::CheckpointFailed { turn, error },
        };
        self.record(&ev).await;
    }

    /// Call the model on the history, or compact it first when it is near the window.
    fn call_model(&mut self) {
        let Some(t) = &mut self.turn else { return };
        t.steered = false;
        let may_compact = !t.compacted;
        let token = t.token.clone();
        if may_compact && self.history.tokens(self.fixed) >= self.agent.cfg.trigger() {
            self.compact();
            return;
        }
        let req = self.request();
        let provider = self.agent.provider.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let send = |ev: Event| {
                let _ = tx.send(ev);
            };
            let delta = |delta: Delta| send(Event::ModelDelta { delta });
            let reply = with_retries(&token, &send, || {
                stream_reply(&*provider, &req, &token, &delta)
            })
            .await;
            send(match reply {
                Ok(Some(c)) => Event::AssistantMessage {
                    content: c.message.content,
                    stop_reason: c.stop_reason,
                    usage: c.usage,
                },
                Ok(None) => Event::ModelCancelled,
                Err(e) => Event::Error {
                    compact: may_compact && matches!(e, ProviderError::ContextOverflow),
                    message: e.to_string(),
                    retry_in_ms: None,
                },
            });
        });
    }

    /// Summarize the history up to where it keeps about `keep` tokens as they are. The
    /// outcome is `Compacted`, `CompactionFailed`, or `ModelCancelled` on an interrupt.
    fn compact(&mut self) {
        let Some(t) = &mut self.turn else { return };
        t.compacted = true;
        let token = t.token.clone();
        let tx = self.tx.clone();
        let cfg = self.agent.cfg;
        let Some((first_kept, dropped)) = self.history.cut(cfg.keep()) else {
            let _ = tx.send(Event::CompactionFailed {
                message: "there is nothing to compact".into(),
            });
            return;
        };
        info!(dropped, "compacting the context");
        let base = self.request();
        let provider = self.agent.provider.clone();
        tokio::spawn(async move {
            let send = |ev: Event| {
                let _ = tx.send(ev);
            };
            let summarize = |req: Request| {
                let (provider, token, send) = (&provider, &token, &send);
                async move {
                    with_retries(token, send, || {
                        stream_reply(&**provider, &req, token, &|_| {})
                    })
                    .await
                }
            };
            let mut reply = summarize(compact::request(&base)).await;
            if matches!(reply, Err(ProviderError::ContextOverflow)) {
                warn!("the summary call is too long too; summarizing a clipped transcript");
                reply = summarize(compact::fallback(&base, cfg.trigger())).await;
            }
            send(match reply {
                Ok(Some(c)) => match compact::summary(&c, dropped) {
                    Ok(summary) => Event::Compacted {
                        summary,
                        first_kept,
                        usage: c.usage,
                    },
                    Err(message) => Event::CompactionFailed { message },
                },
                Ok(None) => Event::ModelCancelled,
                Err(e) => Event::CompactionFailed {
                    message: e.to_string(),
                },
            });
        });
    }

    fn request(&self) -> Request {
        let cfg = self.agent.cfg;
        Request {
            model: cfg.model.to_string(),
            system: Some(cfg.system.to_string()),
            messages: self.history.messages(),
            tools: self.agent.tools.specs().to_vec(),
            effort: cfg.effort,
        }
    }

    /// Read-only tools run concurrently, mutating tools serially in call
    /// order, all under the turn's token. Each result is sent as it
    /// completes; the history restores call order.
    fn run_tools(&mut self, calls: Vec<Call>) {
        let Some(t) = &mut self.turn else { return };
        t.pending = calls.iter().map(|c| c.id.clone()).collect();
        let tools = self.agent.tools.clone();
        let ctx = self.ctx.clone();
        let token = t.token.clone();
        let tx = self.tx.clone();
        let mirror = self.agent.mirror;
        tokio::spawn(async move {
            let send = |call: &Call, out: ToolOutput| {
                info!(
                    tool = %call.name,
                    ok = !out.is_error,
                    chars = out.content.len(),
                    "{} {}",
                    if out.is_error { "✗" } else { "✓" },
                    call.name
                );
                let _ = tx.send(Event::ToolResult {
                    call_id: call.id.clone(),
                    name: call.name.clone(),
                    // a secret it printed stays out of the history and the log
                    content: ctx.secrets.redact(&out.content),
                    is_error: out.is_error,
                    meta: out.meta,
                });
            };

            let mut parallel: JoinSet<(Call, ToolOutput)> = JoinSet::new();
            let mut serial: Vec<Call> = Vec::new();
            for call in calls {
                info!(tool = %call.name, "▶ {} {}", call.name, summarize(&call.input));
                let Some(tool) = tools.get(&call.name) else {
                    send(
                        &call,
                        ToolOutput::error(format!("Unknown tool: {}", call.name)),
                    );
                    continue;
                };
                if let Some(err) = call.input.get("__error") {
                    send(
                        &call,
                        ToolOutput::error(format!("Invalid JSON in tool input: {err}")),
                    );
                    continue;
                }
                if tool.read_only() {
                    let (ctx, token) = (ctx.clone(), token.clone());
                    parallel.spawn(async move {
                        let out = run_one(&*tool, &call, &ctx, token).await;
                        (call, out)
                    });
                } else {
                    serial.push(call);
                }
            }
            for call in &serial {
                let tool = tools.get(&call.name).expect("checked above");
                let out = run_one(&*tool, call, &ctx, token.clone()).await;
                // what it changed is in the log before its result is
                if let Some(mirror) = mirror {
                    let why = format!("{} {}", call.name, call.id);
                    for ev in mirror.lock().await.snapshot(&why).await {
                        let _ = tx.send(ev);
                    }
                }
                send(call, out);
            }
            while let Some(joined) = parallel.join_next().await {
                match joined {
                    Ok((call, out)) => send(&call, out),
                    Err(e) => warn!("tool task failed: {e}"),
                }
            }
        });
    }
}

/// Make `call` until it succeeds or fails for good: each retryable failure is sent as an
/// `Error` and the next try waits for [`retry_delay`]. `Ok(None)` means `token` was cancelled.
async fn with_retries<T, Fut>(
    token: &CancellationToken,
    send: &impl Fn(Event),
    mut call: impl FnMut() -> Fut,
) -> Result<Option<T>, ProviderError>
where
    Fut: Future<Output = Result<Option<T>, ProviderError>>,
{
    let mut attempt = 0;
    loop {
        let e = match call().await {
            Err(e) if e.retryable() => e,
            done => return done,
        };
        let wait = retry_delay(&e, attempt);
        send(Event::Error {
            message: e.to_string(),
            retry_in_ms: Some(wait.as_millis() as u64),
            compact: false,
        });
        tokio::select! {
            biased;
            _ = token.cancelled() => return Ok(None),
            _ = tokio::time::sleep(wait) => {}
        }
        attempt = attempt.saturating_add(1);
    }
}

/// One model call, its deltas going to `on_delta`. `Ok(None)` means `token` was cancelled.
async fn stream_reply(
    provider: &dyn Provider,
    req: &Request,
    token: &CancellationToken,
    on_delta: &(dyn Fn(Delta) + Sync),
) -> Result<Option<Completion>, ProviderError> {
    let mut stream = tokio::select! {
        biased;
        _ = token.cancelled() => return Ok(None),
        r = provider.stream(req) => r?,
    };
    let mut acc = Accumulator::default();
    loop {
        tokio::select! {
            biased;
            _ = token.cancelled() => return Ok(None),
            item = stream.next() => match item {
                Some(delta) => {
                    let delta = delta?;
                    acc.push(delta.clone());
                    on_delta(delta);
                }
                None => return Ok(Some(acc.finish())),
            }
        }
    }
}

/// The provider's retry-after when it gave one, else 1s doubling per attempt up to a minute.
fn retry_delay(e: &ProviderError, attempt: u32) -> Duration {
    match e {
        ProviderError::RateLimited {
            retry_after: Some(d),
        } => *d,
        _ => Duration::from_secs(1 << attempt.min(6)).min(Duration::from_secs(60)),
    }
}

fn tool_calls(content: &[ContentBlock]) -> Vec<Call> {
    content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolCall { id, name, input } => Some(Call {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            }),
            _ => None,
        })
        .collect()
}

async fn run_one(
    tool: &dyn crate::tools::Tool,
    call: &Call,
    ctx: &ToolCtx,
    token: CancellationToken,
) -> ToolOutput {
    if token.is_cancelled() {
        return ToolOutput::error(ABORTED_TOOL_RESULT);
    }
    if tool.read_only() {
        // Read-only tools ignore the token, so race them against it.
        tokio::select! {
            out = tool.call(call.input.clone(), ctx, token.clone()) => out,
            _ = token.cancelled() => ToolOutput::error(ABORTED_TOOL_RESULT),
        }
    } else {
        // Mutating tools (Bash) honour the token themselves and report how far they got.
        tool.call(call.input.clone(), ctx, token).await
    }
}

fn log_event(ev: &Event) {
    match ev {
        Event::AssistantMessage { content, usage, .. } => {
            let text = crate::provider::Message::assistant(content.clone()).text();
            let calls = content
                .iter()
                .filter(|b| matches!(b, ContentBlock::ToolCall { .. }))
                .count();
            info!(
                input = usage.input,
                output = usage.output,
                cache_read = usage.cache_read,
                tool_calls = calls,
                "assistant: {text}"
            );
        }
        Event::ModelCancelled => info!("model call cancelled"),
        Event::Error {
            message,
            retry_in_ms: Some(ms),
            ..
        } => warn!("error: {message}; retrying in {ms} ms"),
        Event::Error {
            message,
            compact: true,
            ..
        } => warn!("error: {message}; compacting"),
        Event::Error { message, .. } => warn!("error: {message}"),
        Event::Compacted { summary, usage, .. } => info!(
            input = usage.input,
            cache_read = usage.cache_read,
            output = usage.output,
            "context compacted ({} chars of summary)",
            summary.len()
        ),
        Event::CompactionFailed { message } => warn!("compaction failed: {message}"),
        Event::TurnEnded { reason, .. } => match reason {
            TurnEndReason::Completed => info!("turn completed"),
            other => warn!("turn ended: {other:?}"),
        },
        Event::PlayerAttached { .. }
        | Event::PlayerDetached { .. }
        | Event::WorkspaceReady { .. }
        | Event::PlayerFailed { .. }
        | Event::WorkspaceRestored { .. }
        | Event::Checkpoint { .. }
        | Event::CheckpointFailed { .. }
        | Event::UserMessage { .. }
        | Event::UserInterrupt { .. }
        | Event::SessionUpdate { .. }
        | Event::TurnStarted { .. }
        | Event::Steered { .. }
        | Event::ToolResult { .. }
        | Event::ModelDelta { .. }
        | Event::Titled { .. }
        | Event::FilesChanged { .. }
        | Event::SecretAnswered { .. }
        | Event::SecretsChanged { .. } => {}
    }
}

/// One-line summary of a tool input for the log.
fn summarize(input: &Value) -> String {
    let s = input
        .get("command")
        .or_else(|| input.get("file_path"))
        .or_else(|| input.get("pattern"))
        .and_then(Value::as_str)
        .map(|s| s.lines().next().unwrap_or("").to_string())
        .unwrap_or_else(|| input.to_string());
    if s.chars().count() > 120 {
        format!("{}…", s.chars().take(120).collect::<String>())
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::ABORT_MARKER;
    use crate::provider::mock::{MockProvider, Script};
    use crate::provider::{Delta, ProviderError, Role, Usage};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    const CFG: AgentConfig = AgentConfig {
        model: "mock",
        system: "sys",
        effort: None,
        context_window: 1_000_000,
        title_model: "mock-small",
        title_effort: None,
    };

    type Published = Arc<Mutex<Vec<(String, Event)>>>;

    /// In-memory session: a log to resume from, user events from a channel, published events
    /// (with the ids they got, `p0`, `p1`, …) into a Vec.
    struct TestSession {
        log: Vec<(String, Event)>,
        inbox: mpsc::Receiver<(String, Event)>,
        published: Published,
    }

    #[async_trait]
    impl Session for TestSession {
        async fn load(&mut self) -> anyhow::Result<Vec<(String, Event)>> {
            Ok(std::mem::take(&mut self.log))
        }
        async fn files(&mut self) -> anyhow::Result<Vec<Event>> {
            Ok(Vec::new())
        }
        async fn recv(&mut self) -> Option<(String, Event)> {
            self.inbox.recv().await
        }
        async fn publish(&mut self, event: &Event) -> String {
            let mut published = self.published.lock().unwrap();
            let id = format!("p{}", published.len());
            published.push((id.clone(), event.clone()));
            id
        }
    }

    struct Harness {
        _dir: tempfile::TempDir,
        workdir: &'static Path,
        provider: Arc<MockProvider>,
        published: Published,
        tx: mpsc::Sender<(String, Event)>,
        detach: watch::Sender<Option<bool>>,
        task: tokio::task::JoinHandle<anyhow::Result<()>>,
    }

    fn harness() -> Harness {
        resumed(Vec::new())
    }

    /// A harness whose session's log already has `log`.
    fn resumed(log: Vec<(String, Event)>) -> Harness {
        resumed_with(log, CFG)
    }

    fn resumed_with(log: Vec<(String, Event)>, cfg: AgentConfig) -> Harness {
        with_checkpoints(log, cfg, None)
    }

    fn with_checkpoints(
        log: Vec<(String, Event)>,
        cfg: AgentConfig,
        checkpoints: Option<&'static Checkpoints>,
    ) -> Harness {
        start(log, cfg, checkpoints, None, Secrets::none())
    }

    /// A harness whose workspace is mirrored, its repository in `state`.
    fn mirrored(state: &Path) -> Harness {
        let state: &'static Path = Box::leak(state.to_path_buf().into_boxed_path());
        start(Vec::new(), CFG, None, Some(state), Secrets::none())
    }

    /// A harness whose session has `secrets`, its workspace mirrored in `state`.
    fn with_secrets(state: &Path, secrets: Secrets) -> Harness {
        let state: &'static Path = Box::leak(state.to_path_buf().into_boxed_path());
        start(Vec::new(), CFG, None, Some(state), secrets)
    }

    fn start(
        log: Vec<(String, Event)>,
        cfg: AgentConfig,
        checkpoints: Option<&'static Checkpoints>,
        mirror: Option<&'static Path>,
        secrets: Secrets,
    ) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let workdir: &'static Path =
            Box::leak(dir.path().canonicalize().unwrap().into_boxed_path());
        let provider = Arc::new(MockProvider::default());
        let published = Arc::new(Mutex::new(Vec::new()));
        let mut agent = Agent {
            provider: provider.clone(),
            cfg,
            tools: Arc::new(Registry::default_set()),
            workdir,
            checkpoints,
            mirror: None,
            secrets: secrets.clone(),
        };
        let (tx, inbox) = mpsc::channel(8);
        let (detach, when) = watch::channel(None);
        let mut session = TestSession {
            log,
            inbox,
            published: published.clone(),
        };
        let task = tokio::spawn(async move {
            if let Some(dir) = mirror {
                let m = Mirror::open(dir, workdir, &[], secrets).await?;
                agent.mirror = Some(Box::leak(Box::new(tokio::sync::Mutex::new(m))));
            }
            let log = session.load().await?;
            agent
                .run(&mut session, log, Detach { epoch: 1, when })
                .await
        });
        Harness {
            _dir: dir,
            workdir,
            provider,
            published,
            tx,
            detach,
            task,
        }
    }

    impl Harness {
        /// A user message, whose id is `in-<text>`.
        async fn send(&self, text: &str) {
            let ev = Event::UserMessage {
                text: text.into(),
                via: None,
            };
            self.tx.send((format!("in-{text}"), ev)).await.unwrap();
        }

        /// A user interrupt for the turn started last.
        async fn interrupt(&self) {
            let turn = self
                .published
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find(|(_, e)| matches!(e, Event::TurnStarted { .. }))
                .map_or("no-turn".to_string(), |(id, _)| id.clone());
            self.interrupt_turn(&turn).await;
        }

        async fn interrupt_turn(&self, turn: &str) {
            let ev = Event::UserInterrupt { turn: turn.into() };
            self.tx.send(("in-interrupt".into(), ev)).await.unwrap();
        }

        fn events(&self) -> Vec<Event> {
            let published = self.published.lock().unwrap();
            published.iter().map(|(_, e)| e.clone()).collect()
        }

        /// Wait until `turns` turns have ended, then disconnect the client.
        async fn finish(self, turns: usize) -> (Vec<String>, Vec<Request>) {
            let ended = || {
                self.events()
                    .iter()
                    .filter(|e| matches!(e, Event::TurnEnded { .. }))
                    .count()
            };
            let deadline = Instant::now() + Duration::from_secs(5);
            while ended() < turns {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for {turns} turns"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let Harness {
                tx,
                task,
                published,
                provider,
                ..
            } = self;
            drop(tx);
            task.await.unwrap().unwrap();
            let events: Vec<_> = published
                .lock()
                .unwrap()
                .iter()
                .map(|(_, e)| e.clone())
                .collect();
            (render(&events), provider.requests())
        }
    }

    /// The text of a message whose id is `<prefix>-<text>`, as the tests make them.
    fn text_of(input: &str) -> &str {
        input.split_once('-').map_or(input, |(_, text)| text)
    }

    /// Compact rendering of the published events: `kind:detail`. A turn's start shows the
    /// message it took, as `user:<text>`.
    fn render(events: &[Event]) -> Vec<String> {
        events
            .iter()
            // titles come when their call does: their own tests look for them
            .filter(|e| !matches!(e, Event::ModelDelta { .. } | Event::Titled { .. }))
            .map(|e| match e {
                Event::PlayerAttached { epoch } => format!("attached:{epoch}"),
                Event::PlayerDetached { epoch } => format!("detached:{epoch}"),
                Event::WorkspaceReady { .. } => "workspace".into(),
                Event::PlayerFailed { error, .. } => format!("failed:{error}"),
                Event::WorkspaceRestored { .. } => "restored".into(),
                Event::Checkpoint { .. } => "checkpoint".into(),
                Event::CheckpointFailed { error, .. } => format!("checkpoint failed:{error}"),
                Event::UserMessage { text, .. } => format!("user:{text}"),
                Event::UserInterrupt { .. } => "interrupt".into(),
                Event::SessionUpdate { session, .. } => format!("update:{session}"),
                Event::TurnStarted { input } => format!("user:{}", text_of(input)),
                Event::Steered { input } => format!("steer:{}", text_of(input)),
                Event::AssistantMessage { content, .. } => {
                    let m = crate::provider::Message::assistant(content.clone());
                    let calls = content
                        .iter()
                        .filter(|b| matches!(b, ContentBlock::ToolCall { .. }))
                        .count();
                    format!("assistant:{}{}", m.text(), "+tool".repeat(calls))
                }
                Event::ModelCancelled => "cancelled".into(),
                Event::ToolResult {
                    name,
                    is_error,
                    content,
                    ..
                } => format!(
                    "tool:{name}:{}:{}",
                    if *is_error { "err" } else { "ok" },
                    content.lines().next().unwrap_or("")
                ),
                Event::Error {
                    message,
                    retry_in_ms: Some(ms),
                    ..
                } => format!("retry:{ms}:{message}"),
                Event::Error {
                    message,
                    compact: true,
                    ..
                } => format!("overflow:{message}"),
                Event::Error { message, .. } => format!("error:{message}"),
                Event::Compacted { summary, .. } => format!("compacted:{summary}"),
                Event::CompactionFailed { message } => format!("compaction-failed:{message}"),
                Event::TurnEnded { reason, .. } => format!("end:{reason:?}"),
                Event::FilesChanged { .. } => "files".into(),
                Event::SecretAnswered { name, provided, .. } => format!("answer:{name}:{provided}"),
                Event::SecretsChanged { names } => format!("secrets:{}", names.join(",")),
                Event::ModelDelta { .. } | Event::Titled { .. } => unreachable!("filtered"),
            })
            .collect()
    }

    /// A model reply that only calls tools.
    fn tool_script(calls: &[(&str, &str, Value)]) -> Script {
        let mut deltas = Vec::new();
        for (i, (id, name, input)) in calls.iter().enumerate() {
            deltas.push(Delta::BlockStart {
                index: i,
                block: ContentBlock::ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    input: input.clone(),
                },
            });
            deltas.push(Delta::BlockStop { index: i });
        }
        deltas.push(Delta::Usage(Usage::default()));
        deltas.push(Delta::Stop(StopReason::ToolUse));
        Script::Deltas {
            delay: Duration::ZERO,
            deltas,
        }
    }

    #[tokio::test]
    async fn one_message_one_reply_with_system_prompt_and_tools() {
        let h = harness();
        h.provider.reset([Script::text("hello", Duration::ZERO)]);
        h.send("hi").await;
        let (kinds, reqs) = h.finish(1).await;
        assert_eq!(kinds, vec!["user:hi", "assistant:hello", "end:Completed"]);
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].system.as_deref(), Some("sys"));
        let names: Vec<_> = reqs[0].tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["Bash", "Read", "Write", "Edit", "Glob", "Grep"]);
    }

    impl Harness {
        /// Wait until a turn has started.
        async fn started(&self) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !self
                .events()
                .iter()
                .any(|e| matches!(e, Event::TurnStarted { .. }))
            {
                assert!(Instant::now() < deadline, "no turn started");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }

        /// Wait for the loop to return on its own (detached), with the client still there.
        async fn detached(self) -> Vec<String> {
            tokio::time::timeout(Duration::from_secs(5), self.task)
                .await
                .expect("the loop didn't return")
                .unwrap()
                .unwrap();
            render(
                &self
                    .published
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|(_, e)| e.clone())
                    .collect::<Vec<_>>(),
            )
        }
    }

    #[tokio::test]
    async fn detaching_lets_the_turn_finish_and_starts_no_more() {
        let h = resumed(vec![sent("a"), sent("b")]);
        h.provider
            .reset([Script::text("one", Duration::from_millis(300))]);
        h.started().await;
        h.detach.send_replace(Some(false));
        assert_eq!(
            h.detached().await,
            ["user:a", "assistant:one", "end:Completed", "detached:1"]
        );
    }

    #[tokio::test]
    async fn detaching_while_idle_returns_at_once() {
        let h = harness();
        h.detach.send_replace(Some(false));
        assert_eq!(h.detached().await, ["detached:1"]);
    }

    #[tokio::test]
    async fn detaching_now_cancels_the_turn() {
        let h = resumed(vec![sent("a")]);
        h.provider
            .reset([Script::text("one", Duration::from_secs(30))]);
        h.started().await;
        h.detach.send_replace(Some(false));
        tokio::time::sleep(Duration::from_millis(50)).await;
        // the graceful stop turns into one that's now
        h.detach.send_replace(Some(true));
        assert_eq!(
            h.detached().await,
            ["user:a", "cancelled", "end:Aborted", "detached:1"]
        );
    }

    #[tokio::test]
    async fn every_turn_ends_with_a_checkpoint_of_the_workspace() {
        let repo = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        let checkpoints: &'static Checkpoints = Box::leak(Box::new(
            crate::checkpoints::tests::checkpoints(repo.path()).await,
        ));
        let h = with_checkpoints(Vec::new(), CFG, Some(checkpoints));
        h.provider.reset([
            Script::text("one", Duration::ZERO),
            Script::text("two", Duration::ZERO),
        ]);
        std::fs::write(h.workdir.join("f"), "x").unwrap();
        h.send("a").await;
        let deadline = Instant::now() + Duration::from_secs(30);
        while !h
            .events()
            .iter()
            .any(|e| matches!(e, Event::Checkpoint { .. }))
        {
            assert!(Instant::now() < deadline, "no checkpoint");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        h.send("b").await;
        let published = h.published.clone();
        let (kinds, _) = h.finish(2).await;
        assert_eq!(
            kinds,
            [
                "user:a",
                "assistant:one",
                "end:Completed",
                "checkpoint",
                "user:b",
                "assistant:two",
                "end:Completed",
                "checkpoint"
            ]
        );
        let published = published.lock().unwrap();
        let started: Vec<_> = published
            .iter()
            .filter(|(_, e)| matches!(e, Event::TurnStarted { .. }))
            .map(|(id, _)| id.clone())
            .collect();
        let checkpointed: Vec<_> = published
            .iter()
            .filter_map(|(_, e)| match e {
                Event::Checkpoint { turn, .. } => Some(turn.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(started, checkpointed);
    }

    #[tokio::test]
    async fn deltas_are_published_before_the_finished_message() {
        let h = harness();
        h.provider.reset([Script::text("hello", Duration::ZERO)]);
        h.send("hi").await;
        let published = h.published.clone();
        let (_, _) = h.finish(1).await;
        let events: Vec<_> = published
            .lock()
            .unwrap()
            .iter()
            .map(|(_, e)| e.clone())
            .collect();
        let deltas: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Event::ModelDelta { delta } => Some(delta.clone()),
                _ => None,
            })
            .collect();
        assert!(deltas.contains(&Delta::Text {
            index: 0,
            text: "hello".into()
        }));
        let last_delta = events
            .iter()
            .rposition(|e| matches!(e, Event::ModelDelta { .. }))
            .unwrap();
        let message = events
            .iter()
            .position(|e| matches!(e, Event::AssistantMessage { .. }))
            .unwrap();
        assert!(last_delta < message);
    }

    #[tokio::test]
    async fn steer_mid_call_is_seen_on_the_next_call() {
        let h = harness();
        h.provider.reset([
            Script::text("r1", Duration::from_millis(300)),
            Script::text("r2", Duration::ZERO),
        ]);
        h.send("m1").await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.send("m2").await;
        let (kinds, reqs) = h.finish(1).await;
        assert_eq!(
            kinds,
            vec![
                "user:m1",
                "steer:m2",
                "assistant:r1",
                "assistant:r2",
                "end:Completed"
            ]
        );
        assert_eq!(reqs.len(), 2);
        let hist: Vec<_> = reqs[1].messages.iter().map(|m| m.text()).collect();
        assert_eq!(hist, vec!["m1", "r1", "m2"]);
    }

    #[tokio::test]
    async fn messages_after_a_turn_start_new_turns() {
        let h = harness();
        h.provider.reset([
            Script::text("r1", Duration::ZERO),
            Script::text("r2", Duration::ZERO),
        ]);
        h.send("m1").await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.send("m2").await;
        let (kinds, reqs) = h.finish(2).await;
        assert_eq!(
            kinds,
            vec![
                "user:m1",
                "assistant:r1",
                "end:Completed",
                "user:m2",
                "assistant:r2",
                "end:Completed",
            ]
        );
        assert_eq!(reqs[1].messages.len(), 3);
    }

    fn overloaded() -> Script {
        Script::Fail {
            delay: Duration::ZERO,
            error: ProviderError::Api {
                status: 529,
                message: "overloaded".into(),
                retryable: true,
            },
        }
    }

    #[tokio::test(start_paused = true)]
    async fn retryable_errors_are_retried_with_backoff_until_a_reply() {
        let h = harness();
        h.provider.reset([
            overloaded(),
            Script::Fail {
                delay: Duration::ZERO,
                error: ProviderError::RateLimited {
                    retry_after: Some(Duration::from_secs(7)),
                },
            },
            overloaded(),
            Script::text("ok now", Duration::ZERO),
        ]);
        h.send("a").await;
        let (k, reqs) = h.finish(1).await;
        assert_eq!(
            k,
            vec![
                "user:a",
                "retry:1000:api error 529: overloaded",
                "retry:7000:rate limited (retry after Some(7s))",
                "retry:4000:api error 529: overloaded",
                "assistant:ok now",
                "end:Completed",
            ]
        );
        assert_eq!(reqs.len(), 4);
        assert!(reqs.iter().all(|r| r.messages == reqs[0].messages));
    }

    #[tokio::test(start_paused = true)]
    async fn an_interrupt_while_waiting_to_retry_aborts_the_turn() {
        let h = harness();
        h.provider.reset([overloaded()]);
        h.send("a").await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.interrupt().await;
        let (k, reqs) = h.finish(1).await;
        assert_eq!(
            k,
            vec![
                "user:a",
                "retry:1000:api error 529: overloaded",
                "cancelled",
                "end:Aborted",
            ]
        );
        assert_eq!(reqs.len(), 1);
    }

    #[test]
    fn backoff_doubles_up_to_a_minute() {
        let e = ProviderError::Stream("cut".into());
        let delays: Vec<u64> = (0..9).map(|n| retry_delay(&e, n).as_secs()).collect();
        assert_eq!(delays, vec![1, 2, 4, 8, 16, 32, 60, 60, 60]);
    }

    #[tokio::test]
    async fn provider_error_fails_turn_and_loop_continues() {
        let h = harness();
        h.provider.reset([
            Script::Fail {
                delay: Duration::ZERO,
                error: ProviderError::Api {
                    status: 500,
                    message: "boom".into(),
                    retryable: false,
                },
            },
            Script::text("ok now", Duration::ZERO),
        ]);
        h.send("a").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        h.send("b").await;
        let (k, reqs) = h.finish(2).await;
        assert_eq!(k[0], "user:a");
        assert!(k[1].starts_with("error:api error 500"));
        assert_eq!(k[2], "end:Failed");
        assert_eq!(k[3], "user:b");
        assert_eq!(k[4], "assistant:ok now");
        // The failed turn left only its user message, so both texts merge into one.
        assert_eq!(reqs[1].messages.len(), 1);
        assert_eq!(
            reqs[1].messages[0].content,
            vec![
                ContentBlock::Text { text: "a".into() },
                ContentBlock::Text { text: "b".into() }
            ]
        );
    }

    #[tokio::test]
    async fn a_secret_reaches_the_tools_and_stays_out_of_the_history_the_log_and_the_files() {
        let state = tempfile::tempdir().unwrap();
        let (tx, secrets) = Secrets::channel(&[("GITHUB_TOKEN", "ghp_first_value")]);
        let h = with_secrets(state.path(), secrets);
        h.provider.reset([
            tool_script(&[(
                "c1",
                "Bash",
                json!({ "command": "printf %s \"$GITHUB_TOKEN\" | tee token.txt" }),
            )]),
            Script::text("done", Duration::ZERO),
        ]);
        h.send("use it").await;
        turns_ended(&h, 1).await;
        let file = std::fs::read_to_string(h.workdir.join("token.txt")).unwrap();
        assert_eq!(file, "ghp_first_value", "the tool had the value");

        // another meanwhile: recorded by name
        tx.send_replace(crate::secrets::tokens(&[
            ("GITHUB_TOKEN", "ghp_first_value"),
            ("NPM_TOKEN", "npm_value_1"),
        ]));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !h
            .events()
            .iter()
            .any(|e| matches!(e, Event::SecretsChanged { .. }))
        {
            assert!(Instant::now() < deadline, "no secrets_changed");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let published = h.published.clone();
        let (kinds, reqs) = h.finish(1).await;
        assert!(
            kinds.contains(&"tool:Bash:ok:[secret:GITHUB_TOKEN]".to_string()),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&"secrets:GITHUB_TOKEN,NPM_TOKEN".to_string()),
            "{kinds:?}"
        );
        let wire = serde_json::to_string(&*published.lock().unwrap()).unwrap();
        assert!(!wire.contains("ghp_first_value"), "a value in the log");
        let model = serde_json::to_string(&reqs[1].messages).unwrap();
        assert!(!model.contains("ghp_first_value"), "a value in the history");
        assert!(model.contains("[secret:GITHUB_TOKEN]"));

        // the mirrored file has it redacted
        let files: Vec<Event> = published
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, e)| matches!(e, Event::FilesChanged { .. }))
            .map(|(_, e)| e.clone())
            .collect();
        let mut bundles = dirigent_sync::Bundles::default();
        let whole: Vec<_> = files.iter().filter_map(|e| bundles.push(e)).collect();
        let reader = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let repo = dirigent_sync::Repo::open(reader.path()).await.unwrap();
        let (last, _) = repo.replay(whole).await;
        repo.checkout(out.path(), &last.unwrap(), true)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(out.path().join("token.txt")).unwrap(),
            "[secret:GITHUB_TOKEN]"
        );
    }

    #[tokio::test]
    async fn an_answer_to_a_request_for_a_secret_starts_a_turn() {
        let h = harness();
        h.provider.reset([Script::text("on it", Duration::ZERO)]);
        let answer = Event::SecretAnswered {
            session: "s1".into(),
            name: "GITHUB_TOKEN".into(),
            provided: true,
            call: "c9".into(),
        };
        h.tx.send(("in-answer".into(), answer)).await.unwrap();
        let (kinds, reqs) = h.finish(1).await;
        assert_eq!(
            kinds,
            vec!["user:answer", "assistant:on it", "end:Completed",]
        );
        let said = serde_json::to_string(&reqs[0].messages).unwrap();
        assert!(
            said.contains("<secret_answer session=\\\"s1\\\" name=\\\"GITHUB_TOKEN\\\">provided</secret_answer>"),
            "{said}"
        );
    }

    #[tokio::test]
    async fn a_tool_that_changes_files_mirrors_them_before_its_result() {
        let state = tempfile::tempdir().unwrap();
        let h = mirrored(state.path());
        let file = h.workdir.join("hello.txt");
        h.provider.reset([
            tool_script(&[(
                "c1",
                "Write",
                json!({ "file_path": file, "content": "hi\n" }),
            )]),
            tool_script(&[("c2", "Read", json!({ "file_path": file }))]),
            tool_script(&[("c3", "Bash", json!({ "command": "true" }))]),
            tool_script(&[("c4", "Bash", json!({ "command": "rm hello.txt" }))]),
            Script::text("done", Duration::ZERO),
        ]);
        h.send("go").await;
        let workdir = h.workdir;
        let published = h.published.clone();
        let (kinds, _) = h.finish(1).await;
        let kinds: Vec<_> = kinds
            .iter()
            .map(|k| k.split(':').take(2).collect::<Vec<_>>().join(":"))
            .collect();
        assert_eq!(
            kinds,
            vec![
                "user:go",
                "assistant:+tool",
                "files",
                "tool:Write",
                "assistant:+tool",
                "tool:Read",
                "assistant:+tool",
                "tool:Bash",
                "assistant:+tool",
                "files",
                "tool:Bash",
                "assistant:done",
                "end:Completed",
            ]
        );

        // the bundles add up to the workspace as each tool left it
        let files: Vec<Event> = published
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, e)| matches!(e, Event::FilesChanged { .. }))
            .map(|(_, e)| e.clone())
            .collect();
        let mut bundles = dirigent_sync::Bundles::default();
        let whole: Vec<_> = files.iter().filter_map(|e| bundles.push(e)).collect();
        assert_eq!(whole.len(), 2);
        assert_eq!(whole[1].parent.as_ref(), Some(&whole[0].commit));
        let reader = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let repo = dirigent_sync::Repo::open(reader.path()).await.unwrap();
        repo.fetch(&whole[0]).await.unwrap();
        repo.checkout(out.path(), &whole[0].commit, true)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(out.path().join("hello.txt")).unwrap(),
            "hi\n"
        );
        repo.fetch(&whole[1]).await.unwrap();
        repo.checkout(out.path(), &whole[1].commit, true)
            .await
            .unwrap();
        assert!(!out.path().join("hello.txt").exists());
        assert!(!workdir.join("hello.txt").exists());
    }

    #[tokio::test]
    async fn tool_round_feeds_results_back() {
        let h = harness();
        let file = h.workdir.join("hello.txt");
        std::fs::write(&file, "hi there\n").unwrap();
        h.provider.reset([
            tool_script(&[("c1", "Read", json!({ "file_path": file }))]),
            Script::text("done", Duration::ZERO),
        ]);
        h.send("read it").await;
        let (kinds, reqs) = h.finish(1).await;
        assert_eq!(
            kinds,
            vec![
                "user:read it",
                "assistant:+tool",
                "tool:Read:ok:     1\thi there",
                "assistant:done",
                "end:Completed",
            ]
        );
        let m = &reqs[1].messages;
        assert_eq!(m.len(), 3);
        assert!(matches!(m[1].content[0], ContentBlock::ToolCall { .. }));
        assert_eq!(m[2].role, Role::User);
        assert!(matches!(
            &m[2].content[0],
            ContentBlock::ToolResult { call_id, is_error: false, .. } if call_id == "c1"
        ));
    }

    #[tokio::test]
    async fn batch_results_fold_in_call_order() {
        let h = harness();
        let a = h.workdir.join("a.txt");
        let b = h.workdir.join("b.txt");
        std::fs::write(&a, "A\n").unwrap();
        std::fs::write(&b, "B\n").unwrap();
        h.provider.reset([
            tool_script(&[
                ("c1", "Bash", json!({ "command": "sleep 0.2; echo bash" })),
                ("c2", "Read", json!({ "file_path": a })),
                ("c3", "Nope", json!({})),
                ("c4", "Read", json!({ "file_path": b })),
                ("c5", "Bash", json!({ "command": "echo second" })),
            ]),
            Script::text("done", Duration::ZERO),
        ]);
        h.send("go").await;
        let (kinds, reqs) = h.finish(1).await;
        // Publish order is completion order; the history restores call order.
        let results: Vec<_> = kinds.iter().filter(|k| k.starts_with("tool:")).collect();
        assert_eq!(results.len(), 5);
        let ids: Vec<_> = reqs[1].messages[2]
            .content
            .iter()
            .map(|b| match b {
                ContentBlock::ToolResult { call_id, .. } => call_id.as_str(),
                _ => "?",
            })
            .collect();
        assert_eq!(ids, vec!["c1", "c2", "c3", "c4", "c5"]);
        assert!(kinds
            .iter()
            .any(|k| k == "tool:Nope:err:Unknown tool: Nope"));
        assert_eq!(kinds.last().unwrap(), "end:Completed");
    }

    #[tokio::test]
    async fn interrupt_during_model_call() {
        let h = harness();
        h.provider.reset([
            Script::text("never", Duration::from_millis(500)),
            Script::text("fresh", Duration::ZERO),
        ]);
        h.send("a").await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.interrupt().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.send("b").await;
        let published = h.published.clone();
        let (kinds, reqs) = h.finish(2).await;
        // the turn's end names the interrupt that ended it
        assert!(published.lock().unwrap().iter().any(|(_, e)| *e
            == Event::TurnEnded {
                reason: TurnEndReason::Aborted,
                interrupt: Some("in-interrupt".into())
            }));
        assert_eq!(
            kinds,
            vec![
                "user:a",
                "cancelled",
                "end:Aborted",
                "user:b",
                "assistant:fresh",
                "end:Completed",
            ]
        );
        // No reply landed, so the aborted turn's text, the marker and the new
        // message merge into one user message.
        assert_eq!(reqs[1].messages.len(), 1);
        assert_eq!(
            reqs[1].messages[0].content,
            vec![
                ContentBlock::Text { text: "a".into() },
                ContentBlock::Text {
                    text: ABORT_MARKER.into()
                },
                ContentBlock::Text { text: "b".into() }
            ]
        );
    }

    #[tokio::test]
    async fn interrupt_during_bash_aborts_tool_and_turn() {
        let h = harness();
        h.provider.reset([
            tool_script(&[("c1", "Bash", json!({ "command": "sleep 5; echo never" }))]),
            Script::text("fresh", Duration::ZERO),
        ]);
        let started = Instant::now();
        h.send("go").await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        h.interrupt().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.send("next").await;
        let (kinds, reqs) = h.finish(2).await;
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(
            kinds,
            vec![
                "user:go",
                "assistant:+tool",
                "tool:Bash:err:Command aborted by user interrupt",
                "end:Aborted",
                "user:next",
                "assistant:fresh",
                "end:Completed",
            ]
        );
        // History is valid: tool_use, its tool_result, then the marker + new message.
        let m = &reqs[1].messages;
        let roles: Vec<_> = m.iter().map(|x| x.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Assistant, Role::User]);
        assert!(matches!(
            m[2].content[0],
            ContentBlock::ToolResult { is_error: true, .. }
        ));
        assert_eq!(m[2].content.len(), 3);
    }

    #[tokio::test]
    async fn an_interrupt_for_another_turn_is_ignored() {
        let h = harness();
        h.provider.reset([
            Script::text("r1", Duration::ZERO),
            Script::text("r2", Duration::from_millis(200)),
        ]);
        h.interrupt().await;
        h.send("a").await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.send("b").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        // the first turn's, arriving while the second runs
        h.interrupt_turn("p0").await;
        let (kinds, _) = h.finish(2).await;
        assert_eq!(
            kinds,
            vec![
                "user:a",
                "assistant:r1",
                "end:Completed",
                "user:b",
                "assistant:r2",
                "end:Completed"
            ]
        );
    }

    #[tokio::test]
    async fn client_disconnect_mid_bash_aborts_and_returns() {
        let h = harness();
        h.provider.reset([tool_script(&[(
            "c1",
            "Bash",
            json!({ "command": "sleep 5; echo never" }),
        )])]);
        let started = Instant::now();
        h.send("go").await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        // Disconnect without waiting for the turn.
        let (kinds, _) = h.finish(0).await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            kinds,
            vec![
                "user:go",
                "assistant:+tool",
                "tool:Bash:err:Command aborted by user interrupt",
                "end:Aborted",
            ]
        );
    }

    fn logged_user(text: &str) -> Event {
        Event::UserMessage {
            text: text.into(),
            via: None,
        }
    }

    fn logged_call(id: &str, name: &str) -> Event {
        Event::AssistantMessage {
            content: vec![ContentBlock::ToolCall {
                id: id.into(),
                name: name.into(),
                input: json!({}),
            }],
            stop_reason: StopReason::ToolUse,
            usage: Usage::default(),
        }
    }

    fn logged_result(id: &str) -> Event {
        Event::ToolResult {
            call_id: id.into(),
            name: "Read".into(),
            content: "out".into(),
            is_error: false,
            meta: Value::Null,
        }
    }

    fn logged_reply(text: &str) -> Event {
        Event::AssistantMessage {
            content: vec![ContentBlock::Text { text: text.into() }],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    fn ended(reason: TurnEndReason) -> Event {
        Event::TurnEnded {
            reason,
            interrupt: None,
        }
    }

    /// A message a user logged, with the id `old-<text>`.
    fn sent(text: &str) -> (String, Event) {
        (format!("old-{text}"), logged_user(text))
    }

    /// The player taking the message `text` for a turn whose id is `t-<text>`.
    fn started(text: &str) -> (String, Event) {
        let input = format!("old-{text}");
        (format!("t-{text}"), Event::TurnStarted { input })
    }

    /// The player's other logged events, with ids of their own.
    fn by_player(events: Vec<Event>) -> Vec<(String, Event)> {
        events
            .into_iter()
            .enumerate()
            .map(|(n, e)| (format!("old-player-{n}"), e))
            .collect()
    }

    #[test]
    fn a_finished_log_needs_no_closing() {
        assert!(close_unfinished(&[], None).is_empty());
        assert!(close_unfinished(
            &by_player(vec![
                logged_user("a"),
                logged_call("c1", "Read"),
                logged_result("c1"),
                ended(TurnEndReason::Completed),
            ]),
            None
        )
        .is_empty());
    }

    #[test]
    fn an_open_turn_gets_aborted_results_then_ends() {
        let closing = close_unfinished(
            &by_player(vec![
                logged_user("a"),
                Event::AssistantMessage {
                    content: vec![
                        ContentBlock::ToolCall {
                            id: "c1".into(),
                            name: "Read".into(),
                            input: json!({}),
                        },
                        ContentBlock::ToolCall {
                            id: "c2".into(),
                            name: "Bash".into(),
                            input: json!({}),
                        },
                    ],
                    stop_reason: StopReason::ToolUse,
                    usage: Usage::default(),
                },
                logged_result("c1"),
            ]),
            None,
        );
        assert_eq!(
            render(&closing),
            vec![
                format!("tool:Bash:err:{ABORTED_TOOL_RESULT}"),
                "end:Aborted".to_string()
            ]
        );
        // a turn cut off before any reply just ends, naming the interrupt it got
        assert_eq!(
            close_unfinished(&by_player(vec![logged_user("a")]), Some("i1".into())),
            vec![Event::TurnEnded {
                reason: TurnEndReason::Aborted,
                interrupt: Some("i1".into())
            }]
        );
    }

    #[tokio::test]
    async fn resume_continues_the_logged_conversation() {
        let mut log = vec![sent("first"), started("first")];
        log.extend(by_player(vec![
            logged_reply("r1"),
            ended(TurnEndReason::Completed),
        ]));
        let h = resumed(log);
        h.provider.reset([Script::text("r2", Duration::ZERO)]);
        h.send("second").await;
        let (kinds, reqs) = h.finish(1).await;
        assert_eq!(kinds, vec!["user:second", "assistant:r2", "end:Completed"]);
        let hist: Vec<_> = reqs[0].messages.iter().map(|m| m.text()).collect();
        assert_eq!(hist, vec!["first", "r1", "second"]);
    }

    #[tokio::test]
    async fn resume_mid_tool_call_closes_the_turn_first() {
        let mut log = vec![sent("go"), started("go")];
        log.extend(by_player(vec![logged_call("c1", "Bash")]));
        log.push((
            "old-interrupt".into(),
            Event::UserInterrupt {
                turn: "t-go".into(),
            },
        ));
        let h = resumed(log);
        h.provider.reset([Script::text("again", Duration::ZERO)]);
        h.send("retry").await;
        let published = h.published.clone();
        let (kinds, reqs) = h.finish(2).await;
        assert_eq!(
            kinds,
            vec![
                format!("tool:Bash:err:{ABORTED_TOOL_RESULT}"),
                "end:Aborted".into(),
                "user:retry".into(),
                "assistant:again".into(),
                "end:Completed".into(),
            ]
        );
        // the closing end names the interrupt the log had for that turn
        assert_eq!(
            published.lock().unwrap()[1].1,
            Event::TurnEnded {
                reason: TurnEndReason::Aborted,
                interrupt: Some("old-interrupt".into())
            }
        );
        // a valid conversation: the call, its aborted result, the marker and the new message
        let m = &reqs[0].messages;
        let roles: Vec<_> = m.iter().map(|x| x.role).collect();
        assert_eq!(roles, vec![Role::User, Role::Assistant, Role::User]);
        assert_eq!(
            m[2].content,
            vec![
                ContentBlock::ToolResult {
                    call_id: "c1".into(),
                    content: ABORTED_TOOL_RESULT.into(),
                    is_error: true
                },
                ContentBlock::Text {
                    text: ABORT_MARKER.into()
                },
                ContentBlock::Text {
                    text: "retry".into()
                },
            ]
        );
    }

    #[tokio::test]
    async fn messages_the_log_left_queued_get_a_turn_each() {
        let mut log = vec![sent("first"), started("first")];
        log.extend(by_player(vec![
            logged_reply("r1"),
            ended(TurnEndReason::Completed),
        ]));
        log.extend([sent("one"), sent("two")]);
        let h = resumed(log);
        h.provider.reset([
            Script::text("r2", Duration::ZERO),
            Script::text("r3", Duration::ZERO),
        ]);
        let (kinds, reqs) = h.finish(2).await;
        assert_eq!(
            kinds,
            vec![
                "user:one",
                "assistant:r2",
                "end:Completed",
                "user:two",
                "assistant:r3",
                "end:Completed"
            ]
        );
        let hist: Vec<_> = reqs[1].messages.iter().map(|m| m.text()).collect();
        assert_eq!(hist, vec!["first", "r1", "one", "r2", "two"]);
    }

    /// Compacts at 8.5k tokens and keeps 1k.
    const SMALL: AgentConfig = AgentConfig {
        context_window: 10_000,
        ..CFG
    };

    const FIRST: &str = "the first message, which a compaction drops. ";

    fn too_long() -> Script {
        Script::Fail {
            delay: Duration::ZERO,
            error: ProviderError::ContextOverflow,
        }
    }

    /// A first turn that a compaction can drop, then a second whose model call reads a file of
    /// `len` characters and goes on with `then`. Returns the rendered Read result.
    async fn read_after_a_turn(h: &Harness, len: usize, then: Vec<Script>) -> String {
        let line = "x".repeat(99);
        let file = h.workdir.join("big.txt");
        std::fs::write(&file, format!("{line}\n").repeat(len / 100)).unwrap();
        let mut scripts = vec![
            Script::text("r1", Duration::ZERO),
            tool_script(&[("c1", "Read", json!({ "file_path": file }))]),
        ];
        scripts.extend(then);
        h.provider.reset(scripts);
        h.send(&FIRST.repeat(10)).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.send("read it").await;
        format!("tool:Read:ok:     1\t{line}")
    }

    #[tokio::test]
    async fn a_context_near_the_window_is_compacted_before_the_call() {
        let h = resumed_with(Vec::new(), SMALL);
        let read = read_after_a_turn(
            &h,
            40_000,
            vec![
                Script::text("SUMMARY", Duration::ZERO),
                Script::text("done", Duration::ZERO),
            ],
        )
        .await;
        let published = h.published.clone();
        let (kinds, reqs) = h.finish(2).await;
        assert_eq!(
            kinds[3..],
            [
                "user:read it",
                "assistant:+tool",
                &read,
                "compacted:SUMMARY",
                "assistant:done",
                "end:Completed"
            ]
        );
        // The summary call is the next call with the instruction appended.
        assert_eq!(reqs.len(), 4);
        let summary_call = &reqs[2];
        assert_eq!(summary_call.tools, reqs[1].tools);
        assert_eq!(summary_call.messages[..3], reqs[1].messages[..]);
        assert_eq!(summary_call.messages.len(), 5);
        assert!(summary_call.messages[4].text().contains("## Goal"));
        // It streams nothing to the log.
        let streamed_summary = published.lock().unwrap().iter().any(|(_, e)| {
            matches!(e, Event::ModelDelta { delta: Delta::Text { text, .. } } if text == "SUMMARY")
        });
        assert!(!streamed_summary);
        // The next call starts from the summary and keeps the read with its call.
        let m = &reqs[3].messages;
        assert_eq!(m.len(), 3);
        assert!(m[0].text().contains("<summary>\nSUMMARY\n</summary>"));
        assert!(!m[0].text().contains(FIRST));
        assert!(matches!(m[1].content[0], ContentBlock::ToolCall { .. }));
        assert!(matches!(m[2].content[0], ContentBlock::ToolResult { .. }));
    }

    #[tokio::test]
    async fn a_call_too_long_for_the_window_compacts_and_calls_again() {
        let h = resumed_with(Vec::new(), SMALL);
        let read = read_after_a_turn(
            &h,
            5_000,
            vec![
                too_long(),
                Script::text("SUMMARY", Duration::ZERO),
                Script::text("done", Duration::ZERO),
            ],
        )
        .await;
        let (kinds, reqs) = h.finish(2).await;
        assert_eq!(
            kinds[3..],
            [
                "user:read it",
                "assistant:+tool",
                &read,
                "overflow:context window exceeded",
                "compacted:SUMMARY",
                "assistant:done",
                "end:Completed"
            ]
        );
        assert!(reqs[4].messages[0].text().contains("SUMMARY"));
    }

    #[tokio::test]
    async fn a_call_still_too_long_after_a_compaction_fails_the_turn() {
        let h = resumed_with(Vec::new(), SMALL);
        let read = read_after_a_turn(
            &h,
            5_000,
            vec![
                too_long(),
                Script::text("SUMMARY", Duration::ZERO),
                too_long(),
            ],
        )
        .await;
        let (kinds, reqs) = h.finish(2).await;
        assert_eq!(
            kinds[3..],
            [
                "user:read it",
                "assistant:+tool",
                &read,
                "overflow:context window exceeded",
                "compacted:SUMMARY",
                "error:context window exceeded",
                "end:Failed"
            ]
        );
        assert_eq!(reqs.len(), 5);
    }

    #[tokio::test]
    async fn a_reply_that_is_no_summary_fails_the_compaction_and_the_call_goes_ahead() {
        let h = resumed_with(Vec::new(), SMALL);
        let read = read_after_a_turn(
            &h,
            5_000,
            vec![
                too_long(),
                tool_script(&[("c2", "Bash", json!({ "command": "ls" }))]),
                Script::text("done", Duration::ZERO),
            ],
        )
        .await;
        let (kinds, _) = h.finish(2).await;
        assert_eq!(
            kinds[3..],
            [
                "user:read it",
                "assistant:+tool",
                &read,
                "overflow:context window exceeded",
                "compaction-failed:the summary call asked for tools",
                "assistant:done",
                "end:Completed"
            ]
        );
    }

    #[tokio::test]
    async fn an_interrupt_during_a_compaction_aborts_the_turn() {
        let h = resumed_with(Vec::new(), SMALL);
        let read = read_after_a_turn(
            &h,
            5_000,
            vec![too_long(), Script::text("SUMMARY", Duration::from_secs(5))],
        )
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.interrupt().await;
        let (kinds, _) = h.finish(2).await;
        assert_eq!(
            kinds[3..],
            [
                "user:read it",
                "assistant:+tool",
                &read,
                "overflow:context window exceeded",
                "cancelled",
                "end:Aborted"
            ]
        );
    }

    #[tokio::test]
    async fn resume_continues_from_the_logged_summary() {
        let mut log = vec![sent("first"), started("first")];
        log.extend(by_player(vec![
            logged_reply("r1"),
            ended(TurnEndReason::Completed),
        ]));
        log.extend([sent("second"), started("second")]);
        log.extend(by_player(vec![
            logged_reply("r2"),
            ended(TurnEndReason::Completed),
            Event::Compacted {
                summary: "SUMMARY".into(),
                first_kept: "t-second".into(),
                usage: Usage::default(),
            },
        ]));
        let h = resumed(log);
        h.provider.reset([Script::text("r3", Duration::ZERO)]);
        h.send("third").await;
        let (_, reqs) = h.finish(1).await;
        let m = &reqs[0].messages;
        assert_eq!(m.len(), 3);
        assert!(m[0].text().contains("<summary>\nSUMMARY\n</summary>"));
        assert!(m[0].text().ends_with("second"));
        assert!(!m[0].text().contains("first"));
        assert_eq!(m[1].text(), "r2");
        assert_eq!(m[2].text(), "third");
    }

    fn titled(title: &str, description: &str, hold: bool) -> Event {
        Event::Titled {
            title: title.into(),
            description: description.into(),
            hold,
        }
    }

    /// The titles the player published, as `(title, description)`.
    fn titles(h: &Harness) -> Vec<(String, String)> {
        h.events()
            .into_iter()
            .filter_map(|e| match e {
                Event::Titled {
                    title,
                    description,
                    hold: false,
                } => Some((title, description)),
                _ => None,
            })
            .collect()
    }

    /// Until the title calls the session made have their outcome.
    async fn named(h: &Harness, calls: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while h.provider.titles().len() < calls {
            assert!(Instant::now() < deadline, "no title call came");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    /// Until `n` turns have ended.
    async fn turns_ended(h: &Harness, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let ended = || {
            h.events()
                .iter()
                .filter(|e| matches!(e, Event::TurnEnded { .. }))
                .count()
        };
        while ended() < n {
            assert!(Instant::now() < deadline, "timed out waiting for {n} turns");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn the_first_message_names_the_session_once() {
        let h = harness();
        h.send("fix the flaky login test please").await;
        named(&h, 1).await;
        turns_ended(&h, 1).await;
        h.send("and the logout one").await;
        turns_ended(&h, 2).await;
        assert_eq!(
            titles(&h),
            [(
                "fix the flaky login test please".to_string(),
                "mock: fix the flaky login test please".to_string()
            )]
        );
        let calls = h.provider.titles();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].model, "mock-small");
        assert!(calls[0].tools.is_empty());
        assert_eq!(
            calls[0].messages[0].text(),
            "<session>\nfix the flaky login test please\n</session>"
        );
        // the turns' calls are theirs alone
        let (kinds, reqs) = h.finish(2).await;
        assert_eq!(kinds.len(), 6);
        assert_eq!(reqs.len(), 2);
    }

    #[tokio::test]
    async fn a_named_session_resumed_is_not_named_again() {
        let mut log = vec![sent("first"), started("first")];
        log.extend(by_player(vec![
            logged_reply("r1"),
            ended(TurnEndReason::Completed),
            titled("First", "The first.", false),
        ]));
        let h = resumed(log);
        h.send("second").await;
        let provider = h.provider.clone();
        h.finish(1).await;
        assert!(provider.titles().is_empty());
    }

    #[tokio::test]
    async fn an_unnamed_session_resumed_is_named_after_its_first_message() {
        let mut log = vec![sent("first"), started("first")];
        log.extend(by_player(vec![
            logged_reply("r1"),
            ended(TurnEndReason::Completed),
        ]));
        log.extend([sent("second"), started("second")]);
        log.extend(by_player(vec![
            logged_reply("r2"),
            ended(TurnEndReason::Completed),
        ]));
        let h = resumed(log);
        named(&h, 1).await;
        assert_eq!(
            titles(&h),
            [("first".to_string(), "mock: first".to_string())]
        );
    }

    #[tokio::test]
    async fn a_compaction_renames_the_session_from_its_summary() {
        let h = resumed_with(Vec::new(), SMALL);
        read_after_a_turn(
            &h,
            40_000,
            vec![
                Script::text("SUMMARY of the work", Duration::ZERO),
                Script::text("done", Duration::ZERO),
            ],
        )
        .await;
        named(&h, 2).await;
        let calls = h.provider.titles();
        let retitle = calls[1].messages[0].text();
        assert!(retitle.starts_with("<current>\nTitle: the first message, which a compaction\n"));
        assert!(retitle.ends_with("<session>\nSUMMARY of the work\n</session>"));
        assert_eq!(titles(&h)[1].0, "SUMMARY of the work");
        h.finish(2).await;
    }

    #[tokio::test]
    async fn a_held_name_is_kept_through_compactions_until_released() {
        let h = resumed_with(Vec::new(), SMALL);
        h.tx.send(("in-rename".into(), titled("Mine", "As I named it.", true)))
            .await
            .unwrap();
        read_after_a_turn(
            &h,
            40_000,
            vec![
                Script::text("SUMMARY", Duration::ZERO),
                Script::text("done", Duration::ZERO),
            ],
        )
        .await;
        turns_ended(&h, 2).await;
        assert!(h.provider.titles().is_empty());
        assert!(titles(&h).is_empty());

        // released, the next compaction names it again, from the name it has
        h.tx.send(("in-release".into(), titled("Mine", "As I named it.", false)))
            .await
            .unwrap();
        let line = "y".repeat(99);
        let file = h.workdir.join("big2.txt");
        std::fs::write(&file, format!("{line}\n").repeat(400)).unwrap();
        h.provider.reset([
            tool_script(&[("c2", "Read", json!({ "file_path": file }))]),
            Script::text("SUMMARY 2", Duration::ZERO),
            Script::text("done 2", Duration::ZERO),
        ]);
        h.send("read the other").await;
        named(&h, 1).await;
        assert!(h.provider.titles()[0].messages[0]
            .text()
            .starts_with("<current>\nTitle: Mine\nDescription: As I named it.\n"));
        assert_eq!(titles(&h)[0].0, "SUMMARY 2");
        h.finish(3).await;
    }

    #[tokio::test]
    async fn a_name_held_while_a_title_call_runs_wins() {
        let h = harness();
        h.provider.reset_titles([Script::text(
            r#"{"title": "Theirs", "description": "The model's."}"#,
            Duration::from_millis(300),
        )]);
        h.send("hello").await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        h.tx.send(("in-rename".into(), titled("Mine", "As I named it.", true)))
            .await
            .unwrap();
        let published = h.published.clone();
        // the session ends only once the call has its outcome
        h.finish(1).await;
        assert!(!published
            .lock()
            .unwrap()
            .iter()
            .any(|(_, e)| matches!(e, Event::Titled { hold: false, .. })));
    }

    #[tokio::test]
    async fn a_failed_title_call_names_nothing_and_the_next_turn_tries_again() {
        let h = harness();
        h.provider.reset_titles([
            Script::Fail {
                delay: Duration::ZERO,
                error: ProviderError::Api {
                    status: 400,
                    message: "no".into(),
                    retryable: false,
                },
            },
            Script::text("Fix the login test", Duration::ZERO),
        ]);
        h.send("one").await;
        named(&h, 1).await;
        turns_ended(&h, 1).await;
        h.send("two").await;
        named(&h, 2).await;
        assert!(titles(&h).is_empty());
        let (kinds, _) = h.finish(2).await;
        assert_eq!(kinds.len(), 6);
        assert_eq!(kinds[5], "end:Completed");
    }
}
