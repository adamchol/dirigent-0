use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;

use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::history::History;
use crate::provider::{Accumulator, ContentBlock, Effort, Provider, Request, StopReason};
use crate::session::Session;
use crate::tools::{Registry, ToolCtx, ToolOutput};
use dirigent_protocol::fold::Fold;
use dirigent_protocol::{Event, TurnEndReason};

#[derive(Debug, Clone, Copy)]
pub struct AgentConfig {
    pub model: &'static str,
    pub system: &'static str,
    pub effort: Option<Effort>,
}

/// Program-lifetime pieces; each session gets its own history and tool context.
pub struct Agent {
    pub provider: Arc<dyn Provider>,
    pub cfg: AgentConfig,
    pub tools: Arc<Registry>,
    pub workdir: &'static Path,
}

pub const ABORTED_TOOL_RESULT: &str = "Tool call aborted by user interrupt";

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
    /// The user interrupt that stopped it, if one did.
    interrupt: Option<String>,
}

type Tx = mpsc::UnboundedSender<Event>;

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
    pub async fn run(&self, session: &mut dyn Session) -> anyhow::Result<()> {
        let log = session.load().await?;
        let mut fold = Fold::default();
        let handled: Vec<Event> = log
            .iter()
            .filter_map(|(id, ev)| fold.apply(id, ev))
            .collect();
        let (tx, mut outcomes) = mpsc::unbounded_channel::<Event>();
        let mut run = Run {
            agent: self,
            session,
            history: History::from_events(&handled),
            ctx: Arc::new(ToolCtx::new(self.workdir)),
            turn: None,
            queue: fold.queued().iter().cloned().collect(),
            tx,
        };
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

        loop {
            run.next_turn().await;
            if !client && run.turn.is_none() {
                break;
            }
            tokio::select! {
                biased;
                Some(ev) = outcomes.recv() => run.handle(ev).await,
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
        info!("session ended");
        Ok(())
    }
}

/// The events that close a turn the handled `events` leave open: an aborted result for each
/// tool call still waiting for one, then `TurnEnded { Aborted }` naming `interrupt`, the
/// interrupt the log has for it. Empty when no turn is open.
fn close_unfinished(events: &[Event], interrupt: Option<String>) -> Vec<Event> {
    let mut open = false;
    let mut waiting: Vec<(String, String)> = Vec::new();
    for ev in events {
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
            Event::UserInterrupt { .. }
            | Event::TurnStarted { .. }
            | Event::Steered { .. }
            | Event::ModelDelta { .. }
            | Event::ModelCancelled
            | Event::Error { .. } => {}
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
    ctx: Arc<ToolCtx>,
    turn: Option<Turn>,
    /// Messages waiting for a turn of their own: `(id, text)`.
    queue: VecDeque<(String, String)>,
    tx: Tx,
}

impl Run<'_> {
    /// An event a user appended, with its id.
    async fn input(&mut self, id: String, ev: Event) {
        match ev {
            Event::UserMessage { text } => {
                if self.turn.is_none() {
                    self.queue.push_back((id, text));
                    return;
                }
                info!("user message (steers the turn)");
                self.session.publish(&Event::Steered { input: id }).await;
                self.history.push(&Event::UserMessage { text });
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
        info!("user message (starts a turn)");
        let id = self.session.publish(&Event::TurnStarted { input }).await;
        self.history.push(&Event::UserMessage { text });
        self.turn = Some(Turn {
            id,
            token: CancellationToken::new(),
            pending: Vec::new(),
            steered: false,
            interrupted: false,
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
                if self.turn.is_none() {
                    return;
                }
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
            Event::Error { .. } => {
                if self.turn.is_some() {
                    self.record(&ev).await;
                    self.end_turn(TurnEndReason::Failed).await;
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
            Event::UserMessage { .. }
            | Event::UserInterrupt { .. }
            | Event::TurnStarted { .. }
            | Event::Steered { .. }
            | Event::TurnEnded { .. } => {}
        }
    }

    /// Publish to the session and keep what the model needs.
    async fn record(&mut self, ev: &Event) {
        log_event(ev);
        self.session.publish(ev).await;
        self.history.push(ev);
    }

    async fn end_turn(&mut self, reason: TurnEndReason) {
        let interrupt = self.turn.take().and_then(|t| t.interrupt);
        self.record(&Event::TurnEnded { reason, interrupt }).await;
    }

    fn call_model(&mut self) {
        let Some(t) = &mut self.turn else { return };
        t.steered = false;
        let cfg = self.agent.cfg;
        let req = Request {
            model: cfg.model.to_string(),
            system: Some(cfg.system.to_string()),
            messages: self.history.messages(),
            tools: self.agent.tools.specs().to_vec(),
            effort: cfg.effort,
        };
        let provider = self.agent.provider.clone();
        let token = t.token.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let send = |ev: Event| {
                let _ = tx.send(ev);
            };
            let opened = tokio::select! {
                biased;
                _ = token.cancelled() => { send(Event::ModelCancelled); return; }
                r = provider.stream(&req) => r,
            };
            let mut stream = match opened {
                Ok(s) => s,
                Err(e) => {
                    send(Event::Error {
                        message: e.to_string(),
                        retryable: e.retryable(),
                    });
                    return;
                }
            };
            let mut acc = Accumulator::default();
            loop {
                tokio::select! {
                    biased;
                    _ = token.cancelled() => { send(Event::ModelCancelled); return; }
                    item = stream.next() => match item {
                        Some(Ok(delta)) => {
                            acc.push(delta.clone());
                            send(Event::ModelDelta { delta });
                        }
                        Some(Err(e)) => {
                            send(Event::Error { message: e.to_string(), retryable: e.retryable() });
                            return;
                        }
                        None => {
                            let c = acc.finish();
                            send(Event::AssistantMessage {
                                content: c.message.content,
                                stop_reason: c.stop_reason,
                                usage: c.usage,
                            });
                            return;
                        }
                    }
                }
            }
        });
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
                    content: out.content,
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
        Event::Error { message, .. } => warn!("error: {message}"),
        Event::TurnEnded { reason, .. } => match reason {
            TurnEndReason::Completed => info!("turn completed"),
            other => warn!("turn ended: {other:?}"),
        },
        Event::UserMessage { .. }
        | Event::UserInterrupt { .. }
        | Event::TurnStarted { .. }
        | Event::Steered { .. }
        | Event::ToolResult { .. }
        | Event::ModelDelta { .. } => {}
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
        task: tokio::task::JoinHandle<anyhow::Result<()>>,
    }

    fn harness() -> Harness {
        resumed(Vec::new())
    }

    /// A harness whose session's log already has `log`.
    fn resumed(log: Vec<(String, Event)>) -> Harness {
        let dir = tempfile::tempdir().unwrap();
        let workdir: &'static Path =
            Box::leak(dir.path().canonicalize().unwrap().into_boxed_path());
        let provider = Arc::new(MockProvider::default());
        let published = Arc::new(Mutex::new(Vec::new()));
        let agent = Agent {
            provider: provider.clone(),
            cfg: CFG,
            tools: Arc::new(Registry::default_set()),
            workdir,
        };
        let (tx, inbox) = mpsc::channel(8);
        let mut session = TestSession {
            log,
            inbox,
            published: published.clone(),
        };
        let task = tokio::spawn(async move { agent.run(&mut session).await });
        Harness {
            _dir: dir,
            workdir,
            provider,
            published,
            tx,
            task,
        }
    }

    impl Harness {
        /// A user message, whose id is `in-<text>`.
        async fn send(&self, text: &str) {
            let ev = Event::UserMessage { text: text.into() };
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
            .filter(|e| !matches!(e, Event::ModelDelta { .. }))
            .map(|e| match e {
                Event::UserMessage { text } => format!("user:{text}"),
                Event::UserInterrupt { .. } => "interrupt".into(),
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
                Event::Error { message, .. } => format!("error:{message}"),
                Event::TurnEnded { reason, .. } => format!("end:{reason:?}"),
                Event::ModelDelta { .. } => unreachable!("filtered"),
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

    #[tokio::test]
    async fn provider_error_fails_turn_and_loop_continues() {
        let h = harness();
        h.provider.reset([
            Script::Fail {
                delay: Duration::ZERO,
                error: ProviderError::Api {
                    status: 500,
                    message: "boom".into(),
                    retryable: true,
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
        Event::UserMessage { text: text.into() }
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
            &[
                logged_user("a"),
                logged_call("c1", "Read"),
                logged_result("c1"),
                ended(TurnEndReason::Completed),
            ],
            None
        )
        .is_empty());
    }

    #[test]
    fn an_open_turn_gets_aborted_results_then_ends() {
        let closing = close_unfinished(
            &[
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
            ],
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
            close_unfinished(&[logged_user("a")], Some("i1".into())),
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
}
