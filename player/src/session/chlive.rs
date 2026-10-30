//! A session in the event system: the log and the inputs are read through chlive, every event is
//! written to Kafka, keyed by the session id (see `dirigent_protocol::rows`).
//!
//! - `load` reads the log (chlive's snapshot is everything in Kafka at subscribe time), then
//!   subscribes to the inputs: those the log doesn't have yet come first, then new ones.
//! - `publish` logs each event with the next `seq`, waiting for Kafka's ack. Deltas go to their
//!   own topic without waiting, tagged with the seq of the last logged event.

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, SystemTime};

use anyhow::Context;
use async_trait::async_trait;
use chlive_client::{ClientOptions, Events, LiveClient, Query};
use futures::StreamExt;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::ClientConfig;
use tracing::{error, warn};

use super::Session;
use dirigent_protocol::rows::{
    timestamp, DeltaRow, EventRow, InputRow, DELTAS, EVENTS, EVENTS_QUERY, INPUTS_QUERY,
};
use dirigent_protocol::Event;

const SEND_TIMEOUT: Duration = Duration::from_secs(30);

pub struct ChliveSession {
    id: String,
    client: LiveClient,
    producer: FutureProducer,
    next_seq: u64,
    /// Inputs already in the log.
    handled: HashSet<String>,
    /// Inputs sent before `load` that the log doesn't have, oldest first.
    pending: VecDeque<Event>,
    /// Inputs sent after `load`; `None` before it and once they've ended.
    inputs: Option<Events<InputRow>>,
}

impl ChliveSession {
    /// `id` is the session's UUID; `chlive_url` its WebSocket (`ws://…/live`), `kafka` the
    /// bootstrap servers.
    pub fn new(id: &str, chlive_url: &str, kafka: &str) -> anyhow::Result<Self> {
        let producer = ClientConfig::new()
            .set("bootstrap.servers", kafka)
            .set("enable.idempotence", "true")
            .set("linger.ms", "0")
            .create()
            .context("creating the Kafka producer")?;
        Ok(ChliveSession {
            id: id.to_string(),
            client: LiveClient::new(ClientOptions::new(chlive_url)),
            producer,
            next_seq: 0,
            handled: HashSet::new(),
            pending: VecDeque::new(),
            inputs: None,
        })
    }

    fn query(&self, sql: &str) -> Query {
        self.client.query(sql).param("session", &self.id)
    }

    /// The event for an input the log doesn't have yet.
    fn input(&self, row: &InputRow) -> Option<Event> {
        if self.handled.contains(&row.input_id) {
            return None;
        }
        match row.event() {
            Ok(ev) => Some(ev),
            Err(e) => {
                warn!(input = row.input_id, "skipping input: {e}");
                None
            }
        }
    }

    fn record<'a>(&'a self, topic: &'a str, payload: &'a str) -> FutureRecord<'a, str, str> {
        FutureRecord::to(topic)
            .key(self.id.as_str())
            .payload(payload)
    }
}

#[async_trait]
impl Session for ChliveSession {
    async fn load(&mut self) -> anyhow::Result<Vec<Event>> {
        let log = self
            .query(EVENTS_QUERY)
            .fetch::<EventRow>()
            .await
            .context("reading the session's log")?;
        let mut events = Vec::with_capacity(log.len());
        for row in log.iter() {
            let ev = row
                .event()
                .with_context(|| format!("event {} of the log", row.seq))?;
            if let Event::UserMessage { input_id, .. } | Event::UserInterrupt { input_id } = &ev {
                self.handled.insert(input_id.clone());
            }
            events.push(ev);
        }
        self.next_seq = log.last().map_or(0, |r| r.seq + 1);

        let (sent, new) = self
            .query(INPUTS_QUERY)
            .history::<InputRow>()
            .await
            .context("subscribing to the session's inputs")?;
        let pending: Vec<Event> = sent.iter().filter_map(|row| self.input(row)).collect();
        self.pending.extend(pending);
        self.inputs = Some(new);
        Ok(events)
    }

    async fn recv(&mut self) -> Option<Event> {
        loop {
            if let Some(ev) = self.pending.pop_front() {
                return Some(ev);
            }
            let row = match self.inputs.as_mut()?.next().await {
                Some(Ok(row)) => row,
                Some(Err(e)) => {
                    error!("the inputs subscription failed: {e}");
                    self.inputs = None;
                    return None;
                }
                None => {
                    self.inputs = None;
                    return None;
                }
            };
            if let Some(ev) = self.input(&row) {
                return Some(ev);
            }
        }
    }

    async fn publish(&mut self, event: &Event) {
        let ts = timestamp(SystemTime::now());
        if let Event::ModelDelta { delta } = event {
            let row = DeltaRow::new(&self.id, self.next_seq.saturating_sub(1), ts, delta);
            let payload = serde_json::to_string(&row).expect("rows serialize");
            if let Err((e, _)) = self.producer.send_result(self.record(DELTAS, &payload)) {
                warn!("dropping a delta: {e}");
            }
            return;
        }
        if let Event::UserMessage { input_id, .. } | Event::UserInterrupt { input_id } = event {
            self.handled.insert(input_id.clone());
        }
        let row = EventRow::new(&self.id, self.next_seq, ts, event);
        self.next_seq += 1;
        let payload = serde_json::to_string(&row).expect("rows serialize");
        if let Err((e, _)) = self
            .producer
            .send(self.record(EVENTS, &payload), SEND_TIMEOUT)
            .await
        {
            error!(
                seq = row.seq,
                kind = row.kind,
                "could not log an event: {e}"
            );
        }
    }
}

/// Against the running stack (`dnvr up`, then `e2e`), on the mock provider; skipped unless
/// `DIRIGENT_E2E` is set. Each test has a session of its own.
#[cfg(test)]
mod e2e {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Instant;

    use dirigent_protocol::rows::{InputRow, INPUTS};
    use dirigent_protocol::{ContentBlock, TurnEndReason};
    use tokio::task::JoinHandle;

    use super::*;
    use crate::agent::{Agent, AgentConfig};
    use crate::provider::mock::MockProvider;
    use crate::provider::Message;
    use crate::tools::Registry;

    struct Stack {
        chlive: String,
        kafka: String,
    }

    fn stack() -> Option<Stack> {
        std::env::var_os("DIRIGENT_E2E")?;
        Some(Stack {
            chlive: std::env::var("CHLIVE_URL").expect("CHLIVE_URL (the `e2e` script sets it)"),
            kafka: std::env::var("DIRIGENT_KAFKA")
                .expect("DIRIGENT_KAFKA (the `e2e` script sets it)"),
        })
    }

    fn new_session() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    struct Player {
        provider: Arc<MockProvider>,
        task: JoinHandle<anyhow::Result<()>>,
        _dir: tempfile::TempDir,
    }

    fn start(stack: &Stack, session: &str) -> Player {
        let dir = tempfile::tempdir().unwrap();
        let workdir: &'static Path =
            Box::leak(dir.path().canonicalize().unwrap().into_boxed_path());
        let provider = Arc::new(MockProvider::default());
        let agent = Agent {
            provider: provider.clone(),
            cfg: AgentConfig {
                model: "mock",
                system: "sys",
                effort: None,
            },
            tools: Arc::new(Registry::default_set()),
            workdir,
        };
        let mut s = ChliveSession::new(session, &stack.chlive, &stack.kafka).unwrap();
        let task = tokio::spawn(async move { agent.run(&mut s).await });
        Player {
            provider,
            task,
            _dir: dir,
        }
    }

    struct User {
        session: String,
        producer: FutureProducer,
    }

    impl User {
        fn new(stack: &Stack, session: &str) -> Self {
            let producer = ClientConfig::new()
                .set("bootstrap.servers", &stack.kafka)
                .create()
                .unwrap();
            User {
                session: session.to_string(),
                producer,
            }
        }

        async fn send(&self, row: InputRow) {
            let payload = serde_json::to_string(&row).unwrap();
            self.producer
                .send(
                    FutureRecord::to(INPUTS)
                        .key(&self.session)
                        .payload(&payload),
                    SEND_TIMEOUT,
                )
                .await
                .map_err(|(e, _)| e)
                .unwrap();
        }

        async fn say(&self, text: &str) {
            let ts = timestamp(SystemTime::now());
            self.send(InputRow::message(
                &self.session,
                new_session(),
                ts,
                text.into(),
            ))
            .await;
        }

        async fn interrupt(&self) {
            let ts = timestamp(SystemTime::now());
            self.send(InputRow::interrupt(&self.session, new_session(), ts))
                .await;
        }
    }

    /// The session's log, read through chlive as it grows, once `done` holds for it.
    async fn log_until(
        stack: &Stack,
        session: &str,
        done: impl Fn(&[EventRow]) -> bool,
    ) -> Vec<EventRow> {
        let client = LiveClient::new(ClientOptions::new(&stack.chlive));
        let mut rows = client
            .query(EVENTS_QUERY)
            .param("session", session)
            .rows::<EventRow>();
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut last = Vec::new();
        loop {
            match tokio::time::timeout_at(deadline.into(), rows.next()).await {
                Ok(Some(Ok(r))) => {
                    last = r.iter().map(|r| r.as_ref().clone()).collect::<Vec<_>>();
                    if done(&last) {
                        return last;
                    }
                }
                Ok(Some(Err(e))) => panic!("log subscription failed: {e}"),
                Ok(None) => panic!("log subscription ended"),
                Err(_) => panic!("timed out; the log has {:#?}", kinds(&last)),
            }
        }
    }

    fn turns_ended(rows: &[EventRow]) -> usize {
        rows.iter().filter(|r| r.kind == "turn_ended").count()
    }

    fn kinds(rows: &[EventRow]) -> Vec<String> {
        rows.iter()
            .map(|r| match r.event().unwrap() {
                Event::UserMessage { text, .. } => format!("user:{text}"),
                Event::AssistantMessage { content, .. } => {
                    let calls = content
                        .iter()
                        .filter(|b| matches!(b, ContentBlock::ToolCall { .. }))
                        .count();
                    format!(
                        "assistant:{}{}",
                        Message::assistant(content).text(),
                        "+tool".repeat(calls)
                    )
                }
                Event::ToolResult { is_error, .. } => {
                    format!("tool:{}", if is_error { "err" } else { "ok" })
                }
                Event::TurnEnded { reason } => format!("end:{reason:?}"),
                _ => r.kind.clone(),
            })
            .collect()
    }

    fn seqs(rows: &[EventRow]) -> Vec<u64> {
        rows.iter().map(|r| r.seq).collect()
    }

    #[tokio::test]
    async fn e2e_a_reply_then_a_resumed_player_continues() {
        let Some(stack) = stack() else { return };
        let session = new_session();
        let user = User::new(&stack, &session);

        let first = start(&stack, &session);
        user.say("hello there").await;
        let log = log_until(&stack, &session, |r| turns_ended(r) == 1).await;
        assert_eq!(
            kinds(&log),
            vec![
                "user:hello there",
                "assistant:mock reply #1 to: hello there",
                "end:Completed"
            ]
        );
        assert_eq!(seqs(&log), vec![0, 1, 2]);
        first.task.abort();

        let second = start(&stack, &session);
        user.say("again").await;
        let log = log_until(&stack, &session, |r| turns_ended(r) == 2).await;
        assert_eq!(seqs(&log), (0..6).collect::<Vec<_>>());
        assert_eq!(
            kinds(&log)[3..],
            [
                "user:again",
                "assistant:mock reply #1 to: again",
                "end:Completed"
            ]
        );
        // the resumed player's model saw the first exchange
        let reqs = second.provider.requests();
        let texts: Vec<_> = reqs[0].messages.iter().map(|m| m.text()).collect();
        assert_eq!(
            texts,
            vec!["hello there", "mock reply #1 to: hello there", "again"]
        );
        second.task.abort();
    }

    #[tokio::test]
    async fn e2e_inputs_sent_before_the_player_starts_are_taken() {
        let Some(stack) = stack() else { return };
        let session = new_session();
        let user = User::new(&stack, &session);
        user.say("early").await;
        let player = start(&stack, &session);
        let log = log_until(&stack, &session, |r| turns_ended(r) == 1).await;
        assert_eq!(
            kinds(&log),
            vec![
                "user:early",
                "assistant:mock reply #1 to: early",
                "end:Completed"
            ]
        );
        player.task.abort();
    }

    #[tokio::test]
    async fn e2e_an_interrupt_aborts_a_running_command() {
        let Some(stack) = stack() else { return };
        let session = new_session();
        let user = User::new(&stack, &session);
        let player = start(&stack, &session);
        user.say("!sleep 30").await;
        log_until(&stack, &session, |r| {
            r.iter().any(|r| r.kind == "assistant_message")
        })
        .await;
        let started = Instant::now();
        user.interrupt().await;
        let log = log_until(&stack, &session, |r| turns_ended(r) == 1).await;
        assert!(started.elapsed() < Duration::from_secs(20));
        let last = log.last().unwrap().event().unwrap();
        assert_eq!(
            last,
            Event::TurnEnded {
                reason: TurnEndReason::Aborted
            }
        );
        assert!(kinds(&log).contains(&"tool:err".to_string()));
        player.task.abort();
    }
}
