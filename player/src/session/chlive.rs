//! A session in the event system: the log is read through chlive, and every event is appended
//! to Kafka, keyed by the session id (see `dirigent_protocol::rows`).
//!
//! - `load` subscribes to the log: chlive's snapshot is everything in Kafka at subscribe time,
//!   and the subscription goes on with what's appended after it.
//! - `recv` passes on what users append; the player's own rows coming back are dropped by id,
//!   since the loop already applied them.
//! - `publish` waits for Kafka's ack, except for deltas, which aren't read back.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use chlive_client::{ClientOptions, Events, LiveClient};
use futures::StreamExt;
use rdkafka::producer::{FutureProducer, FutureRecord};
use rdkafka::ClientConfig;
use tracing::{error, warn};

use super::Session;
use dirigent_protocol::rows::{EventRow, EVENTS, LOG_QUERY, PLAYER};
use dirigent_protocol::Event;

const SEND_TIMEOUT: Duration = Duration::from_secs(30);

pub struct ChliveSession {
    id: String,
    client: LiveClient,
    producer: FutureProducer,
    /// Rows this player appended that haven't come back yet.
    own: HashSet<String>,
    /// The log after `load`; `None` before it and once it's ended.
    log: Option<Events<EventRow>>,
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
            own: HashSet::new(),
            log: None,
        })
    }
}

#[async_trait]
impl Session for ChliveSession {
    async fn load(&mut self) -> anyhow::Result<Vec<(String, Event)>> {
        let (rows, live) = self
            .client
            .query(LOG_QUERY)
            .param("session", &self.id)
            .history::<EventRow>()
            .await
            .context("subscribing to the session's log")?;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows.iter() {
            let ev = row
                .event()
                .with_context(|| format!("the log's row at offset {}", row.offset))?;
            events.push((row.id.clone(), ev));
        }
        self.log = Some(live);
        Ok(events)
    }

    async fn recv(&mut self) -> Option<(String, Event)> {
        loop {
            let row = match self.log.as_mut()?.next().await {
                Some(Ok(row)) => row,
                Some(Err(e)) => {
                    error!("the log subscription failed: {e}");
                    self.log = None;
                    return None;
                }
                None => {
                    self.log = None;
                    return None;
                }
            };
            if self.own.remove(&row.id) {
                continue;
            }
            match row.event() {
                Ok(ev) => return Some((row.id.clone(), ev)),
                Err(e) => warn!(offset = row.offset, "skipping a row: {e}"),
            }
        }
    }

    async fn publish(&mut self, event: &Event) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let row = EventRow::new(&self.id, id.clone(), PLAYER, event);
        let payload = serde_json::to_string(&row).expect("rows serialize");
        let record = FutureRecord::to(EVENTS)
            .key(self.id.as_str())
            .payload(&payload);
        if let Event::ModelDelta { .. } = event {
            if let Err((e, _)) = self.producer.send_result(record) {
                warn!("dropping a delta: {e}");
            }
            return id;
        }
        self.own.insert(id.clone());
        if let Err((e, _)) = self.producer.send(record, SEND_TIMEOUT).await {
            error!(kind = row.kind, "could not append an event: {e}");
        }
        id
    }
}

/// Against the running stack (`dnvr up`, then `e2e`), on the mock provider; skipped unless
/// `DIRIGENT_E2E` is set. Each test has a session of its own.
#[cfg(test)]
mod e2e {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Instant;

    use dirigent_protocol::rows::USER;
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

    fn new_id() -> String {
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

        async fn send(&self, event: Event) {
            let row = EventRow::new(&self.session, new_id(), USER, &event);
            let payload = serde_json::to_string(&row).unwrap();
            self.producer
                .send(
                    FutureRecord::to(EVENTS)
                        .key(&self.session)
                        .payload(&payload),
                    SEND_TIMEOUT,
                )
                .await
                .map_err(|(e, _)| e)
                .unwrap();
        }

        async fn say(&self, text: &str) {
            self.send(Event::UserMessage { text: text.into() }).await;
        }

        async fn interrupt(&self, turn: &str) {
            self.send(Event::UserInterrupt { turn: turn.into() }).await;
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
            .query(LOG_QUERY)
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
                Event::UserMessage { text } => format!("user:{text}"),
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
                Event::TurnEnded { reason, .. } => format!("end:{reason:?}"),
                _ => r.kind.clone(),
            })
            .collect()
    }

    /// Every row has the Kafka fields, in offset order, also when it came live.
    fn assert_kafka_fields(rows: &[EventRow]) {
        assert!(
            rows.windows(2).all(|w| w[0].offset < w[1].offset),
            "offsets out of order: {:?}",
            rows.iter().map(|r| r.offset).collect::<Vec<_>>()
        );
        assert!(rows.iter().all(|r| !r.ts.is_empty()), "a row has no ts");
    }

    #[tokio::test]
    async fn e2e_a_reply_then_a_resumed_player_continues() {
        let Some(stack) = stack() else { return };
        let session = new_id();
        let user = User::new(&stack, &session);

        let first = start(&stack, &session);
        user.say("hello there").await;
        let log = log_until(&stack, &session, |r| turns_ended(r) == 1).await;
        assert_eq!(
            kinds(&log),
            vec![
                "user:hello there",
                "turn_started",
                "assistant:mock reply #1 to: hello there",
                "end:Completed"
            ]
        );
        assert_kafka_fields(&log);
        first.task.abort();

        let second = start(&stack, &session);
        user.say("again").await;
        let log = log_until(&stack, &session, |r| turns_ended(r) == 2).await;
        assert_eq!(
            kinds(&log)[4..],
            [
                "user:again",
                "turn_started",
                "assistant:mock reply #1 to: again",
                "end:Completed"
            ]
        );
        assert_kafka_fields(&log);
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
    async fn e2e_messages_sent_before_the_player_starts_get_a_turn_each() {
        let Some(stack) = stack() else { return };
        let session = new_id();
        let user = User::new(&stack, &session);
        user.say("one").await;
        user.say("two").await;
        let player = start(&stack, &session);
        let log = log_until(&stack, &session, |r| turns_ended(r) == 2).await;
        assert_eq!(
            kinds(&log),
            vec![
                "user:one",
                "user:two",
                "turn_started",
                "assistant:mock reply #1 to: one",
                "end:Completed",
                "turn_started",
                "assistant:mock reply #2 to: two",
                "end:Completed"
            ]
        );
        player.task.abort();
    }

    #[tokio::test]
    async fn e2e_an_interrupt_aborts_a_running_command() {
        let Some(stack) = stack() else { return };
        let session = new_id();
        let user = User::new(&stack, &session);
        let player = start(&stack, &session);
        user.say("!sleep 30").await;
        let log = log_until(&stack, &session, |r| {
            r.iter().any(|r| r.kind == "assistant_message")
        })
        .await;
        let turn = log.iter().find(|r| r.kind == "turn_started").unwrap();
        let started = Instant::now();
        user.interrupt(&turn.id).await;
        let log = log_until(&stack, &session, |r| turns_ended(r) == 1).await;
        assert!(started.elapsed() < Duration::from_secs(20));
        let interrupt = log.iter().find(|r| r.kind == "user_interrupt").unwrap();
        assert_eq!(
            log.last().unwrap().event().unwrap(),
            Event::TurnEnded {
                reason: TurnEndReason::Aborted,
                interrupt: Some(interrupt.id.clone()),
            }
        );
        assert!(kinds(&log).contains(&"tool:err".to_string()));
        player.task.abort();
    }
}
