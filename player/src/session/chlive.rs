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
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{error, warn};

use super::control::player_token;
use super::Session;
use dirigent_protocol::control::pb;
use dirigent_protocol::rows::{EventRow, EVENTS, FILES_QUERY, LOG_QUERY};
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
    /// (`ws://…/live`), where the player is who its lease's `tokens` say.
    pub fn new(
        id: &str,
        epoch: i64,
        chlive_url: &str,
        tokens: watch::Receiver<pb::Tokens>,
    ) -> Self {
        let opts = ClientOptions::new(chlive_url).credentials(player_token(tokens));
        let client = LiveClient::new(opts);
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
            &Event::PlayerAttached {
                epoch: self.epoch,
                version: Some(crate::VERSION.into()),
            },
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

    async fn files(&mut self) -> anyhow::Result<Vec<Event>> {
        let rows = self
            .client
            .query(FILES_QUERY)
            .param("session", &self.id)
            .complete()
            .fetch::<EventRow>()
            .await
            .context("reading the session's workspace")?;
        let mut files = Vec::with_capacity(rows.len());
        for row in rows.iter() {
            match row.event() {
                Ok(ev) => files.push(ev),
                Err(e) => warn!(offset = row.offset, "skipping a row of the workspace: {e}"),
            }
        }
        Ok(files)
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
        // the log the loop follows leaves the workspace out: those never come back
        if !matches!(event, Event::FilesChanged { .. }) {
            self.own.insert(id.clone());
        }
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

    use crate::dirigent::Director;
    use dirigent_protocol::control::{Part, Source};
    use dirigent_protocol::{ContentBlock, TurnEndReason, Update};
    use tokio::task::JoinHandle;

    use super::*;
    use crate::agent::{Agent, AgentConfig, Detach};
    use crate::checkpoints::Checkpoints;
    use crate::provider::mock::{MockProvider, Script};
    use crate::provider::Message;
    use crate::secrets::Secrets;
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
        put_session(stack, serde_json::json!({ "source": source })).await
    }

    /// A new session, [`USER`]'s, made with `body` (its spec, viewers).
    async fn put_session(stack: &Stack, body: serde_json::Value) -> String {
        let session = new_id();
        let status = api(stack, reqwest::Method::PUT, &format!("/sessions/{session}"))
            .json(&body)
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, reqwest::StatusCode::CREATED);
        session
    }

    /// A request to control's API, as [`USER`].
    fn api(stack: &Stack, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .request(method, format!("{}{path}", stack.control))
            .header("dirigent-user", USER)
    }

    struct Player {
        provider: Arc<MockProvider>,
        lease: Lease,
        task: JoinHandle<anyhow::Result<()>>,
        /// A dirigent's follower.
        follower: Option<JoinHandle<()>>,
        dir: tempfile::TempDir,
        _state: tempfile::TempDir,
    }

    impl Player {
        /// Stops it and gives the session back.
        async fn stop(self) {
            self.task.abort();
            if let Some(f) = self.follower {
                f.abort();
            }
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
            Checkpoints::connect(&stack.kopia, state.path(), Some(session))
                .await
                .unwrap(),
        ));
        let provider = Arc::new(MockProvider::default());
        let player = new_id();
        let lease = Lease::acquire(&stack.control_ws, session, &player, None)
            .await
            .unwrap();
        let (tools, follower) = match lease.spec.part {
            Part::Coder => (Registry::default_set(), None),
            Part::Dirigent => {
                let d = Director::new(session, lease.tokens(), &stack.chlive, &stack.control);
                let d = Arc::new(d.unwrap());
                let follow = crate::dirigent::follow(d.clone(), stack.chlive.clone());
                (Registry::new(d.tools()), Some(tokio::spawn(follow)))
            }
        };
        let mut agent = Agent {
            provider: provider.clone(),
            cfg: AgentConfig {
                model: "mock",
                system: "sys",
                effort: None,
                context_window: 1_000_000,
                title_model: "mock",
                title_effort: None,
            },
            tools: Arc::new(tools),
            workdir,
            checkpoints: Some(checkpoints),
            mirror: None,
            secrets: Secrets::new(lease.tokens()),
        };
        let mirror = state.path().join("sync");
        let secrets = Secrets::new(lease.tokens());
        let mut s = ChliveSession::new(session, lease.epoch, &stack.chlive, lease.tokens());
        let source = lease.spec.source.clone();
        let detach = Detach {
            epoch: lease.epoch,
            when: lease.detach(),
        };
        let task = tokio::spawn(async move {
            let (log, m) = workspace::open(
                &mut s,
                workdir,
                source.as_ref(),
                Some(checkpoints),
                &mirror,
                secrets,
            )
            .await?;
            agent.mirror = m.map(|m| &*Box::leak(Box::new(tokio::sync::Mutex::new(m))));
            agent.run(&mut s, log, detach).await
        });
        Player {
            provider,
            lease,
            task,
            follower,
            dir,
            _state: state,
        }
    }

    /// chlive, as `user`, with the credentials control signs for them (`declared` auth).
    fn client(stack: &Stack, user: &str) -> LiveClient {
        let (control, user) = (stack.control.clone(), user.to_string());
        let token = move || dev_token(control.clone(), user.clone());
        LiveClient::new(ClientOptions::new(&stack.chlive).credentials(token))
    }

    async fn dev_token(control: String, user: String) -> Result<String, reqwest::Error> {
        let got: serde_json::Value = reqwest::Client::new()
            .post(format!("{control}/token"))
            .header("dirigent-user", user)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(got["token"].as_str().unwrap_or_default().to_string())
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
            self.send(Event::UserMessage {
                text: text.into(),
                via: None,
            })
            .await;
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
        log_within(stack, session, Duration::from_secs(60), done).await
    }

    /// The same, within `within`.
    async fn log_within(
        stack: &Stack,
        session: &str,
        within: Duration,
        done: impl Fn(&[EventRow]) -> bool,
    ) -> Vec<EventRow> {
        let client = client(stack, USER);
        let mut rows = client
            .query(LOG_QUERY)
            .param("session", session)
            .rows::<EventRow>();
        let deadline = Instant::now() + within;
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

    /// The session's workspace in its log, read as [`USER`].
    async fn files(stack: &Stack, session: &str) -> Vec<Event> {
        client(stack, USER)
            .query(FILES_QUERY)
            .param("session", session)
            .complete()
            .fetch::<EventRow>()
            .await
            .unwrap()
            .iter()
            .map(|r| r.event().unwrap())
            .collect()
    }

    /// The epochs of the players that attached, in order.
    fn attached(rows: &[EventRow]) -> Vec<i64> {
        rows.iter()
            .filter_map(|r| match r.event().unwrap() {
                Event::PlayerAttached { epoch, .. } => Some(epoch),
                _ => None,
            })
            .collect()
    }

    fn turns_ended(rows: &[EventRow]) -> usize {
        rows.iter().filter(|r| r.kind == "turn_ended").count()
    }

    /// What happened, leaving out the players attaching, their workspaces, and the session's
    /// titles (which come when their call does).
    fn kinds(rows: &[EventRow]) -> Vec<String> {
        rows.iter()
            .filter(|r| {
                !["player_attached", "workspace_ready", "checkpoint", "titled"]
                    .contains(&r.kind.as_str())
            })
            .map(|r| match r.event().unwrap() {
                Event::UserMessage { text, .. } => format!("user:{text}"),
                Event::AssistantMessage { content, .. } => {
                    let calls = content
                        .iter()
                        .filter(|b| matches!(b, ContentBlock::ToolCall { .. }))
                        .count();
                    // its first line: `--mock`'s replies go on with random paragraphs
                    let text = Message::assistant(content).text();
                    format!(
                        "assistant:{}{}",
                        text.lines().next().unwrap_or_default(),
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
        let stop = api(
            &stack,
            reqwest::Method::POST,
            &format!("/sessions/{session}/stop"),
        );
        assert!(stop.send().await.unwrap().status().is_success());
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
        let row = EventRow::new(
            &session,
            new_id(),
            &Event::UserMessage {
                text: "x".into(),
                via: None,
            },
        );
        let e = outsider.insert(EVENTS, [row]).await.unwrap_err();
        assert_eq!(e.code, "INVALID_INSERT", "{e}");
    }

    #[tokio::test]
    async fn e2e_an_owner_writes_only_inputs_and_a_viewer_nothing() {
        let Some(stack) = stack() else { return };
        let viewer = "user:e2e-viewer";
        let session = put_session(&stack, serde_json::json!({ "viewers": [viewer] })).await;
        let owner = client(&stack, USER);
        let insert = |c: &LiveClient, ev: Event| {
            let row = EventRow::new(&session, new_id(), &ev);
            let c = c.clone();
            async move {
                // until chlive has the grants (over CDC), refusals may be the lag's
                let started = Instant::now();
                loop {
                    match c.insert(EVENTS, [&row]).await {
                        Ok(_) => return Ok(()),
                        Err(e) if started.elapsed() > Duration::from_secs(5) => return Err(e),
                        Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
                    }
                }
            }
        };
        let say = |text: &str| Event::UserMessage {
            text: text.into(),
            via: None,
        };
        insert(&owner, say("hi")).await.unwrap();
        insert(&owner, Event::UserInterrupt { turn: "t".into() })
            .await
            .unwrap();
        let renamed = Event::Titled {
            title: "Mine".into(),
            description: "As I named it.".into(),
            hold: true,
        };
        insert(&owner, renamed).await.unwrap();
        let forged = Event::AssistantMessage {
            content: vec![ContentBlock::Text {
                text: "forged".into(),
            }],
            stop_reason: dirigent_protocol::StopReason::EndTurn,
            usage: Default::default(),
        };
        let e = insert(&owner, forged).await.unwrap_err();
        assert!(e.message.contains("kind"), "{e}");
        let files = Event::FilesChanged {
            commit: "c".into(),
            parent: None,
            part: 0,
            parts: 1,
            bundle: String::new(),
        };
        let e = insert(&owner, files).await.unwrap_err();
        assert!(e.message.contains("kind"), "{e}");
        let as_viewer = client(&stack, viewer);
        let e = insert(&as_viewer, say("me too")).await.unwrap_err();
        assert!(e.message.contains("session_id"), "{e}");
        // and reads it
        let rows = as_viewer
            .query(LOG_QUERY)
            .param("session", &session)
            .complete()
            .fetch::<EventRow>()
            .await
            .unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[tokio::test]
    async fn e2e_the_player_names_the_session_and_its_owner_renames_it() {
        let Some(stack) = stack() else { return };
        let session = new_session(&stack).await;
        let user = User::new(&stack, &session);
        let titles = |rows: &[EventRow]| -> Vec<(String, Event)> {
            rows.iter()
                .filter(|r| r.kind == "titled")
                .map(|r| (r.author.clone(), r.event().unwrap()))
                .collect()
        };

        let first = start(&stack, &session).await;
        user.say("fix the flaky login test").await;
        let log = log_until(&stack, &session, |r| {
            turns_ended(r) == 1 && !titles(r).is_empty()
        })
        .await;
        let named = titles(&log);
        assert_eq!(named.len(), 1);
        assert_ne!(named[0].0, USER);
        assert_eq!(
            named[0].1,
            Event::Titled {
                title: "fix the flaky login test".into(),
                description: "mock: fix the flaky login test".into(),
                hold: false,
            }
        );
        assert_eq!(first.provider.titles().len(), 1);

        // the owner's name, held, is the one a resumed player goes on with
        let renamed = Event::Titled {
            title: "Login flake".into(),
            description: "Why the login test fails one run in ten.".into(),
            hold: true,
        };
        user.send(renamed.clone()).await;
        log_until(&stack, &session, |r| titles(r).len() == 2).await;
        first.stop().await;
        let second = start(&stack, &session).await;
        user.say("again").await;
        let log = log_until(&stack, &session, |r| turns_ended(r) == 2).await;
        let named = titles(&log);
        assert_eq!(named.len(), 2);
        assert_eq!((named[1].0.as_str(), &named[1].1), (USER, &renamed));
        assert!(second.provider.titles().is_empty());
        second.stop().await;
    }

    #[tokio::test]
    async fn e2e_chlive_takes_control_signed_credentials_only_and_a_players_for_its_session() {
        let Some(stack) = stack() else { return };
        // a credential's signature over another's claims
        let mine = dev_token(stack.control.clone(), USER.into()).await.unwrap();
        let theirs = dev_token(stack.control.clone(), "user:e2e-other".into())
            .await
            .unwrap();
        let (claims, signature) = (mine.rsplit_once('.').unwrap().0, theirs.rsplit('.').next());
        let forged = format!("{claims}.{}", signature.unwrap());
        let (tx, mut refused) = mpsc::unbounded_channel();
        let opts = ClientOptions::new(&stack.chlive)
            .credentials(move || std::future::ready(Ok::<_, &str>(forged.clone())))
            .on_auth_error(move |e| {
                let _ = tx.send(e.code.clone());
            });
        let forger = LiveClient::new(opts);
        let session = new_session(&stack).await;
        let _sub = forger
            .query(LOG_QUERY)
            .param("session", &session)
            .rows::<EventRow>();
        let code = tokio::time::timeout(Duration::from_secs(10), refused.recv()).await;
        assert_eq!(code.unwrap().as_deref(), Some("AUTH_BAD_SIGNATURE"));

        // a player's are for its session only
        let other = new_session(&stack).await;
        let lease = Lease::acquire(&stack.control_ws, &session, &new_id(), None)
            .await
            .unwrap();
        let opts = ClientOptions::new(&stack.chlive).credentials(player_token(lease.tokens()));
        let player = LiveClient::new(opts);
        let attached = |s: &str| {
            EventRow::new(
                s,
                new_id(),
                &Event::PlayerAttached {
                    epoch: 1,
                    version: None,
                },
            )
        };
        let started = Instant::now();
        // until chlive has the lease's grant (over CDC)
        while player.insert(EVENTS, [attached(&session)]).await.is_err() {
            assert!(
                started.elapsed() < GRANT_WAIT,
                "chlive never let the player in"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let e = player.insert(EVENTS, [attached(&other)]).await.unwrap_err();
        assert!(e.message.contains("session_id"), "{e}");
        lease.release().await;
    }

    #[tokio::test]
    async fn e2e_a_dirigent_creates_a_session_and_hears_when_its_turn_ends() {
        let Some(stack) = stack() else { return };
        let dirigent = put_session(&stack, serde_json::json!({ "part": "dirigent" })).await;
        let user = User::new(&stack, &dirigent);
        let player = start(&stack, &dirigent).await;
        user.say(r#"!!create_session {"message": "hi"}"#).await;
        let log = log_until(&stack, &dirigent, |r| {
            r.iter().any(|r| r.kind == "session_update")
        })
        .await;
        let child = log
            .iter()
            .find_map(|r| match r.event().unwrap() {
                Event::ToolResult { meta, .. } => {
                    meta["directs"]["session"].as_str().map(String::from)
                }
                _ => None,
            })
            .expect("a session made");
        // the child is the user's, its task from them via the dirigent
        let child_log = log_until(&stack, &child, |r| turns_ended(r) == 1).await;
        let task = child_log.iter().find(|r| r.kind == "user_message").unwrap();
        assert_eq!(task.author, USER);
        assert_eq!(
            task.event().unwrap(),
            Event::UserMessage {
                text: "hi".into(),
                via: Some(dirigent.clone())
            }
        );
        let update = log.iter().find(|r| r.kind == "session_update").unwrap();
        let Event::SessionUpdate {
            session,
            update: Update::TurnEnded { reason },
            reply: Some(reply),
            ..
        } = update.event().unwrap()
        else {
            panic!("{update:?}")
        };
        assert_eq!(
            (session.as_str(), reason),
            (child.as_str(), TurnEndReason::Completed)
        );
        assert!(reply.contains("hi"), "{reply}");
        // the update starts a turn of the dirigent's
        log_until(&stack, &dirigent, |r| turns_ended(r) == 2).await;
        // a new player doesn't tell it again
        player.stop().await;
        let again = start(&stack, &dirigent).await;
        log_until(&stack, &dirigent, |r| attached(r).len() == 2).await;
        tokio::time::sleep(Duration::from_secs(3)).await;
        let log = log_until(&stack, &dirigent, |r| attached(r).len() == 2).await;
        assert_eq!(log.iter().filter(|r| r.kind == "session_update").count(), 1);
        again.stop().await;
        let stop = api(
            &stack,
            reqwest::Method::POST,
            &format!("/sessions/{child}/stop"),
        );
        assert!(stop.send().await.unwrap().status().is_success());
    }

    #[tokio::test]
    async fn e2e_a_secret_given_mid_session_reaches_its_tools_and_stays_out_of_the_log() {
        let Some(stack) = stack() else { return };
        let session = new_session(&stack).await;
        let user = User::new(&stack, &session);
        let player = start(&stack, &session).await;
        user.say("!true").await;
        log_until(&stack, &session, |r| turns_ended(r) == 1).await;

        let value = format!("e2e-secret-{}", new_id());
        let put = api(
            &stack,
            reqwest::Method::PUT,
            &format!("/sessions/{session}/secrets/E2E_TOKEN"),
        )
        .json(&serde_json::json!({ "value": value }));
        assert_eq!(
            put.send().await.unwrap().status(),
            reqwest::StatusCode::NO_CONTENT
        );
        let log = log_until(&stack, &session, |r| {
            r.iter().any(|r| r.kind == "secrets_changed")
        })
        .await;
        let changed = log.iter().find(|r| r.kind == "secrets_changed").unwrap();
        assert_eq!(
            changed.event().unwrap(),
            Event::SecretsChanged {
                names: vec!["E2E_TOKEN".into()]
            }
        );

        user.say(r#"!printf %s "$E2E_TOKEN" > f.txt && cat f.txt"#)
            .await;
        let log = log_until(&stack, &session, |r| turns_ended(r) == 2).await;
        let said = log
            .iter()
            .rev()
            .find_map(|r| match r.event().unwrap() {
                Event::ToolResult { content, .. } => Some(content),
                _ => None,
            })
            .unwrap();
        assert_eq!(said, "[secret:E2E_TOKEN]");
        assert!(
            log.iter().all(|r| !r.data.contains(&value)),
            "a value in the log"
        );
        let on_disk = std::fs::read_to_string(player.dir.path().join("f.txt")).unwrap();
        assert_eq!(on_disk, value);
        // and in the workspace's mirror, redacted
        let files = files(&stack, &session).await;
        assert!(files
            .iter()
            .all(|e| !serde_json::to_string(e).unwrap().contains(&value)));
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
            std::fs::read_to_string(out.path().join("f.txt")).unwrap(),
            "[secret:E2E_TOKEN]"
        );
        player.stop().await;
    }

    #[tokio::test]
    async fn e2e_a_dirigent_asks_for_secrets_only_for_its_owners_sessions_and_hears_the_answer() {
        let Some(stack) = stack() else { return };
        let dirigent = put_session(&stack, serde_json::json!({ "part": "dirigent" })).await;
        let mine = new_session(&stack).await;
        // another's, which the user only views
        let theirs = new_id();
        let status = reqwest::Client::new()
            .put(format!("{}/sessions/{theirs}", stack.control))
            .header("dirigent-user", "user:e2e-someone-else")
            .json(&serde_json::json!({ "viewers": [USER] }))
            .send()
            .await
            .unwrap()
            .status();
        assert_eq!(status, reqwest::StatusCode::CREATED);
        let user = User::new(&stack, &dirigent);
        let player = start(&stack, &dirigent).await;
        let ask = |session: &str| {
            format!(
                r#"!!request_secret {{"session": "{session}", "name": "GITHUB_TOKEN", "why": "to push"}}"#
            )
        };
        user.say(&ask(&mine)).await;
        log_until(&stack, &dirigent, |r| turns_ended(r) == 1).await;
        user.say(&ask(&theirs)).await;
        let log = log_until(&stack, &dirigent, |r| turns_ended(r) == 2).await;
        let results: Vec<(bool, String)> = log
            .iter()
            .filter_map(|r| match r.event().unwrap() {
                Event::ToolResult {
                    is_error, content, ..
                } => Some((is_error, content)),
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 2);
        assert!(
            !results[0].0 && results[0].1.contains("Asked the user"),
            "{results:?}"
        );
        assert!(
            results[1].0 && results[1].1.contains("only view"),
            "{results:?}"
        );

        // the owner's answer, written by them, starts the dirigent's next turn
        user.send(Event::SecretAnswered {
            session: mine.clone(),
            name: "GITHUB_TOKEN".into(),
            provided: true,
            call: "c1".into(),
        })
        .await;
        let log = log_until(&stack, &dirigent, |r| turns_ended(r) == 3).await;
        let answer = log.iter().find(|r| r.kind == "secret_answered").unwrap();
        assert_eq!(answer.author, USER);
        player.stop().await;
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
        user.say("!echo mine > notes.txt").await;
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
            "mine\n"
        );
        assert_eq!(std::fs::read_to_string(dir.join("file")).unwrap(), "main\n");

        // the workspace in the log: the clone, then the note; the restored workspace is as the
        // log has it, so the next change builds on the note
        let before = files(&stack, &session).await;
        assert_eq!(before.len(), 2, "{before:?}");
        user.say("!echo more > more.txt").await;
        log_until(&stack, &session, |r| turns_ended(r) == 2).await;
        let after = files(&stack, &session).await;
        assert_eq!(after.len(), 3);
        let parent = |e: &Event| match e {
            Event::FilesChanged { commit, parent, .. } => (commit.clone(), parent.clone()),
            _ => unreachable!(),
        };
        assert_eq!(parent(&after[2]).1, Some(parent(&before[1]).0));
        second.stop().await;
    }
}
