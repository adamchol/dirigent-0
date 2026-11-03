//! A session in the event system: the log is read through chlive, and every event is inserted
//! into it through chlive, as the player's subject (see `dirigent_protocol::rows`). chlive lets
//! the player do both while it holds the session's lease (its grants come from control).
//!
//! - `load` first inserts a [`Event::PlayerAttached`], again until chlive has the lease's grant,
//!   then subscribes to the log: chlive's snapshot is everything in Kafka at subscribe time, and
//!   the subscription goes on with what's appended after it.
//! - `recv` passes on what users append; the player's own rows coming back are dropped by id,
//!   since the loop already applied them.
//! - `publish` waits for chlive's ack, except for deltas, which aren't read back. One insert is
//!   in flight at a time, carrying every row queued meanwhile, so rows go in in order.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::Context;
use async_trait::async_trait;
use chlive_client::{ClientOptions, Events, LiveClient};
use futures::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, warn};

use super::Session;
use dirigent_protocol::rows::{EventRow, EVENTS, LOG_QUERY};
use dirigent_protocol::Event;

const SEND_TIMEOUT: Duration = Duration::from_secs(30);
/// How long chlive may take to see the lease's grant (over CDC from Postgres).
const GRANT_WAIT: Duration = Duration::from_secs(30);
/// chlive's `max_insert_rows`.
const MAX_ROWS: usize = 1000;

pub struct ChliveSession {
    id: String,
    epoch: i64,
    client: LiveClient,
    writes: mpsc::UnboundedSender<Write>,
    /// Rows this player appended that haven't come back yet.
    own: HashSet<String>,
    /// The log after `load`; `None` before it and once it's ended.
    log: Option<Events<EventRow>>,
}

/// A row to insert, and who waits for it to be in.
struct Write {
    row: EventRow,
    done: Option<oneshot::Sender<()>>,
}

impl ChliveSession {
    /// `id` is the session's UUID, held as the `epoch`th player; `chlive_url` its WebSocket
    /// (`ws://…/live`), where the player is `subject`.
    pub fn new(id: &str, epoch: i64, chlive_url: &str, subject: &str) -> Self {
        let client = LiveClient::new(ClientOptions::new(chlive_url).user(subject));
        let (writes, queue) = mpsc::unbounded_channel();
        tokio::spawn(write(client.clone(), queue));
        ChliveSession {
            id: id.to_string(),
            epoch,
            client,
            writes,
            own: HashSet::new(),
            log: None,
        }
    }

    /// Insert `PlayerAttached`, again until chlive takes it: then it has the lease's grant,
    /// for inserts and reads alike.
    async fn attach(&mut self) -> anyhow::Result<()> {
        let id = uuid::Uuid::new_v4().to_string();
        let row = EventRow::new(
            &self.id,
            id.clone(),
            &Event::PlayerAttached { epoch: self.epoch },
        );
        let started = Instant::now();
        loop {
            match self.client.insert(EVENTS, [&row]).await {
                Ok(_) => break,
                // refused (INVALID_INSERT) until chlive has the grant
                Err(_) if started.elapsed() < GRANT_WAIT => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(e) => return Err(e).context("chlive never let the player in"),
            }
        }
        self.own.insert(id);
        Ok(())
    }
}

/// Inserts the queued rows, one insert at a time.
async fn write(client: LiveClient, mut queue: mpsc::UnboundedReceiver<Write>) {
    while let Some(first) = queue.recv().await {
        let mut batch = vec![first];
        while batch.len() < MAX_ROWS {
            match queue.try_recv() {
                Ok(w) => batch.push(w),
                Err(_) => break,
            }
        }
        let rows = batch.iter().map(|w| &w.row);
        if let Err(e) = client.insert(EVENTS, rows).await {
            let kinds: Vec<_> = batch.iter().map(|w| w.row.kind.as_str()).collect();
            error!(?kinds, "could not append events: {e}");
        }
        for w in batch {
            if let Some(done) = w.done {
                let _ = done.send(());
            }
        }
    }
}

#[async_trait]
impl Session for ChliveSession {
    async fn load(&mut self) -> anyhow::Result<Vec<(String, Event)>> {
        self.attach().await?;
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
            self.own.remove(&row.id);
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
        let row = EventRow::new(&self.id, id.clone(), event);
        if let Event::ModelDelta { .. } = event {
            let _ = self.writes.send(Write { row, done: None });
            return id;
        }
        self.own.insert(id.clone());
        let (done, wait) = oneshot::channel();
        let kind = row.kind.clone();
        let _ = self.writes.send(Write {
            row,
            done: Some(done),
        });
        if tokio::time::timeout(SEND_TIMEOUT, wait).await.is_err() {
            error!(kind, "timed out appending an event");
        }
        id
    }
}

/// Against the running stack (`dnvr up`, then `e2e`), on the mock provider; skipped unless
/// `DIRIGENT_E2E` is set. Each test has a session of its own, made through control, with
/// [`USER`] its user.
#[cfg(test)]
mod e2e {
    use std::path::Path;
    use std::sync::Arc;

    use dirigent_protocol::control::{player_subject, Source};
    use dirigent_protocol::{ContentBlock, TurnEndReason};
    use tokio::task::JoinHandle;

    use super::*;
    use crate::agent::{Agent, AgentConfig, Detach};
    use crate::checkpoints::Checkpoints;
    use crate::provider::mock::{MockProvider, Script};
    use crate::provider::Message;
    use crate::session::control::{Lease, Refused};
    use crate::tools::Registry;
    use crate::workspace;

    const USER: &str = "user:e2e";

    #[derive(Clone)]
    struct Stack {
        chlive: String,
        /// Control's HTTP API, and its WebSocket for players.
        control: String,
        control_ws: String,
        /// The kopia repository players checkpoint to.
        kopia: String,
    }

    fn stack() -> Option<Stack> {
        std::env::var_os("DIRIGENT_E2E")?;
        let var = |name: &str| {
            std::env::var(name).unwrap_or_else(|_| panic!("{name} (the `e2e` script sets it)"))
        };
        Some(Stack {
            chlive: var("CHLIVE_URL"),
            control: var("DIRIGENT_CONTROL_HTTP"),
            control_ws: var("DIRIGENT_CONTROL_URL"),
            kopia: var("DIRIGENT_KOPIA_TOKEN"),
        })
    }

    fn new_id() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// A new session, with [`USER`] its user.
    async fn new_session(stack: &Stack) -> String {
        new_session_from(stack, None).await
    }

    /// A new session, idle (the test starts its players), with [`USER`] its user and its workspace
    /// from `source`.
    async fn new_session_from(stack: &Stack, source: Option<Source>) -> String {
        let session = new_id();
        let status = reqwest::Client::new()
            .put(format!("{}/sessions/{session}", stack.control))
            .json(&serde_json::json!({ "users": [USER], "source": source }))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, reqwest::StatusCode::CREATED);
        session
    }

    struct Player {
        provider: Arc<MockProvider>,
        lease: Lease,
        task: JoinHandle<anyhow::Result<()>>,
        dir: tempfile::TempDir,
        _state: tempfile::TempDir,
    }

    impl Player {
        /// Stops it and gives the session back.
        async fn stop(self) {
            self.task.abort();
            self.lease.release().await;
        }
    }

    /// A player on `session`, once it holds it.
    async fn start(stack: &Stack, session: &str) -> Player {
        let dir = tempfile::tempdir().unwrap();
        let workdir: &'static Path =
            Box::leak(dir.path().canonicalize().unwrap().into_boxed_path());
        let state = tempfile::tempdir().unwrap();
        let checkpoints: &'static Checkpoints = Box::leak(Box::new(
            Checkpoints::connect(&stack.kopia, state.path(), session)
                .await
                .unwrap(),
        ));
        let provider = Arc::new(MockProvider::default());
        let agent = Agent {
            provider: provider.clone(),
            cfg: AgentConfig {
                model: "mock",
                system: "sys",
                effort: None,
                context_window: 1_000_000,
            },
            tools: Arc::new(Registry::default_set()),
            workdir,
            checkpoints: Some(checkpoints),
        };
        let player = new_id();
        let lease = Lease::acquire(&stack.control_ws, session, &player, None)
            .await
            .unwrap();
        let mut s = ChliveSession::new(
            session,
            lease.epoch,
            &stack.chlive,
            &player_subject(&player),
        );
        let source = lease.spec.source.clone();
        let detach = Detach {
            epoch: lease.epoch,
            when: lease.detach(),
        };
        let task = tokio::spawn(async move {
            let log = workspace::open(&mut s, workdir, source.as_ref(), Some(checkpoints)).await?;
            agent.run(&mut s, log, detach).await
        });
        Player {
            provider,
            lease,
            task,
            dir,
            _state: state,
        }
    }

    fn client(stack: &Stack, user: &str) -> LiveClient {
        LiveClient::new(ClientOptions::new(&stack.chlive).user(user))
    }

    struct User {
        session: String,
        client: LiveClient,
    }

    impl User {
        fn new(stack: &Stack, session: &str) -> Self {
            User {
                session: session.to_string(),
                client: client(stack, USER),
            }
        }

        async fn send(&self, event: Event) {
            let row = EventRow::new(&self.session, new_id(), &event);
            self.client.insert(EVENTS, [row]).await.unwrap();
        }

        async fn say(&self, text: &str) {
            self.send(Event::UserMessage { text: text.into() }).await;
        }

        async fn interrupt(&self, turn: &str) {
            self.send(Event::UserInterrupt { turn: turn.into() }).await;
        }
    }

    /// The session's log, read through chlive as [`USER`] as it grows, once `done` holds for it.
    async fn log_until(
        stack: &Stack,
        session: &str,
        done: impl Fn(&[EventRow]) -> bool,
    ) -> Vec<EventRow> {
        let client = client(stack, USER);
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

    /// The epochs of the players that attached, in order.
    fn attached(rows: &[EventRow]) -> Vec<i64> {
        rows.iter()
            .filter_map(|r| match r.event().unwrap() {
                Event::PlayerAttached { epoch } => Some(epoch),
                _ => None,
            })
            .collect()
    }

    fn turns_ended(rows: &[EventRow]) -> usize {
        rows.iter().filter(|r| r.kind == "turn_ended").count()
    }

    /// What happened, leaving out the players attaching and their workspaces.
    fn kinds(rows: &[EventRow]) -> Vec<String> {
        rows.iter()
            .filter(|r| {
                !["player_attached", "workspace_ready", "checkpoint"].contains(&r.kind.as_str())
            })
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
        let session = new_session(&stack).await;
        let user = User::new(&stack, &session);

        let first = start(&stack, &session).await;
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
        first.stop().await;

        let second = start(&stack, &session).await;
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
        assert_eq!(attached(&log), [1, 2]);
        // the resumed player's model saw the first exchange
        let reqs = second.provider.requests();
        let texts: Vec<_> = reqs[0].messages.iter().map(|m| m.text()).collect();
        assert_eq!(
            texts,
            vec!["hello there", "mock reply #1 to: hello there", "again"]
        );
        second.stop().await;
    }

    #[tokio::test]
    async fn e2e_messages_sent_before_the_player_starts_get_a_turn_each() {
        let Some(stack) = stack() else { return };
        let session = new_session(&stack).await;
        let user = User::new(&stack, &session);
        user.say("one").await;
        user.say("two").await;
        let player = start(&stack, &session).await;
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
        player.stop().await;
    }

    #[tokio::test]
    async fn e2e_an_interrupt_aborts_a_running_command() {
        let Some(stack) = stack() else { return };
        let session = new_session(&stack).await;
        let user = User::new(&stack, &session);
        let player = start(&stack, &session).await;
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
        player.stop().await;
    }

    #[tokio::test]
    async fn e2e_a_second_player_is_refused_while_the_first_holds_the_session() {
        let Some(stack) = stack() else { return };
        let session = new_session(&stack).await;
        let user = User::new(&stack, &session);
        let first = start(&stack, &session).await;
        log_until(&stack, &session, |r| attached(r) == [1]).await;
        let refused = Lease::acquire(&stack.control_ws, &session, &new_id(), None).await;
        let err = refused.err().expect("the second player got a held session");
        assert!(matches!(err.downcast_ref(), Some(Refused::Held)), "{err:#}");
        first.stop().await;
        let second = start(&stack, &session).await;
        user.say("hi").await;
        let log = log_until(&stack, &session, |r| turns_ended(r) == 1).await;
        assert_eq!(attached(&log), [1, 2]);
        assert_eq!(second.provider.requests().len(), 1);
        second.stop().await;
    }

    #[tokio::test]
    async fn e2e_stopping_the_session_detaches_the_player() {
        let Some(stack) = stack() else { return };
        let session = new_session(&stack).await;
        let user = User::new(&stack, &session);
        let first = start(&stack, &session).await;
        first
            .provider
            .reset([Script::text("one", Duration::from_secs(1))]);
        user.say("hi").await;
        log_until(&stack, &session, |r| {
            r.iter().any(|r| r.kind == "turn_started")
        })
        .await;
        let http = reqwest::Client::new();
        let control = |action: &str| {
            http.post(format!("{}/sessions/{session}/{action}", stack.control))
                .send()
        };
        assert!(control("stop").await.unwrap().status().is_success());
        let log = log_until(&stack, &session, |r| {
            r.iter().any(|r| r.kind == "player_detached")
        })
        .await;
        assert_eq!(
            kinds(&log),
            [
                "user:hi",
                "turn_started",
                "assistant:one",
                "end:Completed",
                "player_detached"
            ]
        );
        let Player { task, lease, .. } = first;
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the player didn't return")
            .unwrap()
            .unwrap();
        lease.release().await;
        // idle: the next player takes it
        let second = start(&stack, &session).await;
        log_until(&stack, &session, |r| attached(r) == [1, 2]).await;
        second.stop().await;
    }

    #[tokio::test]
    async fn e2e_a_user_without_access_reads_and_writes_nothing() {
        let Some(stack) = stack() else { return };
        let session = new_session(&stack).await;
        let user = User::new(&stack, &session);
        user.say("private").await;
        log_until(&stack, &session, |r| r.len() == 1).await;
        let outsider = client(&stack, "user:outsider");
        let rows = outsider
            .query(LOG_QUERY)
            .param("session", &session)
            .fetch::<EventRow>()
            .await
            .unwrap();
        assert_eq!(rows.len(), 0);
        let row = EventRow::new(&session, new_id(), &Event::UserMessage { text: "x".into() });
        let e = outsider.insert(EVENTS, [row]).await.unwrap_err();
        assert_eq!(e.code, "INVALID_INSERT", "{e}");
    }

    #[tokio::test]
    async fn e2e_the_player_clones_the_source_into_its_workspace() {
        let Some(stack) = stack() else { return };
        let repos = tempfile::tempdir().unwrap();
        let source = Source::Git {
            url: workspace::tests::repo(repos.path()),
            reference: None,
        };
        let session = new_session_from(&stack, Some(source.clone())).await;
        let player = start(&stack, &session).await;
        let log = log_until(&stack, &session, |r| {
            r.iter().any(|r| r.kind == "workspace_ready")
        })
        .await;
        let ready = log.iter().find(|r| r.kind == "workspace_ready").unwrap();
        let Event::WorkspaceReady {
            source: Some(got),
            head: Some(head),
            recreated: false,
        } = ready.event().unwrap()
        else {
            panic!("{ready:?}");
        };
        assert_eq!(got, source);
        assert_eq!(head.len(), 40);
        assert_eq!(
            std::fs::read_to_string(player.dir.path().join("file")).unwrap(),
            "main\n"
        );
        player.stop().await;
    }

    #[tokio::test]
    async fn e2e_a_player_on_a_new_machine_restores_the_last_checkpoint() {
        let Some(stack) = stack() else { return };
        let repos = tempfile::tempdir().unwrap();
        let source = Source::Git {
            url: workspace::tests::repo(repos.path()),
            reference: None,
        };
        let session = new_session_from(&stack, Some(source)).await;
        let user = User::new(&stack, &session);
        let first = start(&stack, &session).await;
        log_until(&stack, &session, |r| {
            r.iter().any(|r| r.kind == "workspace_ready")
        })
        .await;
        std::fs::write(first.dir.path().join("notes.txt"), "mine").unwrap();
        user.say("hi").await;
        let log = log_until(&stack, &session, |r| {
            r.iter().any(|r| r.kind == "checkpoint")
        })
        .await;
        let snapshot = log
            .iter()
            .find_map(|r| match r.event().unwrap() {
                Event::Checkpoint { snapshot, .. } => Some(snapshot),
                _ => None,
            })
            .unwrap();
        first.stop().await;

        let second = start(&stack, &session).await;
        let log = log_until(&stack, &session, |r| {
            r.iter().any(|r| r.kind == "workspace_restored")
        })
        .await;
        assert!(log.iter().any(|r| r.event().unwrap()
            == Event::WorkspaceRestored {
                snapshot: snapshot.clone()
            }));
        let dir = second.dir.path();
        assert_eq!(
            std::fs::read_to_string(dir.join("notes.txt")).unwrap(),
            "mine"
        );
        assert_eq!(std::fs::read_to_string(dir.join("file")).unwrap(), "main\n");
        second.stop().await;
    }
}
