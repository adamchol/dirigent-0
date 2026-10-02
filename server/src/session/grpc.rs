//! gRPC transport: one bidirectional stream is one session. Only one client
//! can be connected at a time; a second one is refused with
//! `FAILED_PRECONDITION`. Nothing is stored, so `load` is always empty.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::{TcpListenerStream, UnboundedReceiverStream};
use tonic::{Request, Response, Status, Streaming};
use tracing::{error, info, warn};

use super::Session;
use crate::event::{Event, TurnEndReason};
use crate::provider::{ContentBlock, Delta, StopReason, Usage};

pub mod pb {
    tonic::include_proto!("dirigent.v1");
}

use pb::dirigent_server::{Dirigent, DirigentServer};

pub struct GrpcServer {
    addr: SocketAddr,
    sessions: mpsc::Receiver<GrpcSession>,
}

impl GrpcServer {
    pub async fn bind(addr: SocketAddr) -> anyhow::Result<Self> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let addr = listener.local_addr()?;
        let (tx, sessions) = mpsc::channel(1);
        let service = Service {
            sessions: tx,
            active: Arc::new(AtomicBool::new(false)),
        };
        tokio::spawn(async move {
            let result = tonic::transport::Server::builder()
                .add_service(DirigentServer::new(service))
                .serve_with_incoming(TcpListenerStream::new(listener))
                .await;
            if let Err(e) = result {
                error!("gRPC server stopped: {e}");
            }
        });
        Ok(GrpcServer { addr, sessions })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Wait for the next client to open a session.
    pub async fn accept(&mut self) -> Option<GrpcSession> {
        self.sessions.recv().await
    }
}

struct Service {
    sessions: mpsc::Sender<GrpcSession>,
    active: Arc<AtomicBool>,
}

/// Clears the "a session is connected" flag when the session is dropped.
struct ActiveGuard(Arc<AtomicBool>);

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

#[tonic::async_trait]
impl Dirigent for Service {
    type SessionStream = UnboundedReceiverStream<Result<pb::Event, Status>>;

    async fn session(
        &self,
        request: Request<Streaming<pb::Input>>,
    ) -> Result<Response<Self::SessionStream>, Status> {
        if self.active.swap(true, Ordering::SeqCst) {
            return Err(Status::failed_precondition(
                "a session is already connected",
            ));
        }
        let guard = ActiveGuard(self.active.clone());
        let (output, rx) = mpsc::unbounded_channel();
        let session = GrpcSession {
            input: request.into_inner(),
            output,
            _guard: guard,
        };
        self.sessions
            .send(session)
            .await
            .map_err(|_| Status::unavailable("shutting down"))?;
        Ok(Response::new(UnboundedReceiverStream::new(rx)))
    }
}

pub struct GrpcSession {
    input: Streaming<pb::Input>,
    output: mpsc::UnboundedSender<Result<pb::Event, Status>>,
    _guard: ActiveGuard,
}

#[async_trait]
impl Session for GrpcSession {
    async fn load(&mut self) -> anyhow::Result<Vec<Event>> {
        Ok(Vec::new())
    }

    async fn recv(&mut self) -> Option<Event> {
        loop {
            match self.input.next().await {
                Some(Ok(input)) => match input.kind {
                    Some(pb::input::Kind::Message(m)) => {
                        return Some(Event::UserMessage { text: m.text })
                    }
                    Some(pb::input::Kind::Interrupt(_)) => return Some(Event::UserInterrupt),
                    None => continue,
                },
                Some(Err(status)) => {
                    warn!("client stream failed: {status}");
                    return None;
                }
                None => {
                    info!("client closed the session");
                    return None;
                }
            }
        }
    }

    async fn publish(&mut self, event: &Event) {
        let _ = self.output.send(Ok(to_proto(event)));
    }
}

// ---------------------------------------------------------------------------
// Conversions
// ---------------------------------------------------------------------------

fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

fn json_string(v: &Value) -> String {
    if v.is_null() {
        String::new()
    } else {
        v.to_string()
    }
}

#[cfg_attr(not(test), allow(dead_code))] // decode side, for resume
fn json_value(s: &str) -> Value {
    if s.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(s).unwrap_or_else(|_| Value::String(s.to_string()))
    }
}

fn stop_reason_str(s: &StopReason) -> String {
    match s {
        StopReason::EndTurn => "end_turn".into(),
        StopReason::MaxTokens => "max_tokens".into(),
        StopReason::ToolUse => "tool_use".into(),
        StopReason::Refusal => "refusal".into(),
        StopReason::ContextOverflow => "context_overflow".into(),
        StopReason::Other { raw } => raw.clone(),
    }
}

#[cfg_attr(not(test), allow(dead_code))] // decode side, for resume
fn stop_reason_from(s: &str) -> StopReason {
    match s {
        "end_turn" => StopReason::EndTurn,
        "max_tokens" => StopReason::MaxTokens,
        "tool_use" => StopReason::ToolUse,
        "refusal" => StopReason::Refusal,
        "context_overflow" => StopReason::ContextOverflow,
        raw => StopReason::Other { raw: raw.into() },
    }
}

fn block_to_proto(b: &ContentBlock) -> Option<pb::ContentBlock> {
    use pb::content_block::Kind;
    let kind = match b {
        ContentBlock::Text { text } => Kind::Text(pb::Text { text: text.clone() }),
        ContentBlock::Thinking {
            text,
            provider_data,
        } => Kind::Thinking(pb::Thinking {
            text: text.clone(),
            provider_data_json: json_string(provider_data),
        }),
        ContentBlock::ToolCall { id, name, input } => Kind::ToolCall(pb::ToolCall {
            id: id.clone(),
            name: name.clone(),
            input_json: json_string(input),
        }),
        // Tool results never appear in assistant content.
        ContentBlock::ToolResult { .. } => return None,
    };
    Some(pb::ContentBlock { kind: Some(kind) })
}

#[cfg_attr(not(test), allow(dead_code))] // decode side, for resume
fn block_from_proto(b: pb::ContentBlock) -> Option<ContentBlock> {
    use pb::content_block::Kind;
    Some(match b.kind? {
        Kind::Text(t) => ContentBlock::Text { text: t.text },
        Kind::Thinking(t) => ContentBlock::Thinking {
            text: t.text,
            provider_data: json_value(&t.provider_data_json),
        },
        Kind::ToolCall(c) => ContentBlock::ToolCall {
            id: c.id,
            name: c.name,
            input: json_value(&c.input_json),
        },
    })
}

fn usage_to_proto(u: &Usage) -> pb::Usage {
    pb::Usage {
        input: u.input,
        output: u.output,
        cache_read: u.cache_read,
        cache_write: u.cache_write,
    }
}

#[cfg_attr(not(test), allow(dead_code))] // decode side, for resume
fn usage_from_proto(u: pb::Usage) -> Usage {
    Usage {
        input: u.input,
        output: u.output,
        cache_read: u.cache_read,
        cache_write: u.cache_write,
    }
}

fn delta_to_proto(d: &Delta) -> pb::ModelDelta {
    use pb::model_delta::Kind;
    let kind = match d {
        Delta::BlockStart { index, block } => Kind::BlockStart(pb::BlockStart {
            index: *index as u32,
            block: block_to_proto(block),
        }),
        Delta::Text { index, text } => Kind::Text(pb::TextDelta {
            index: *index as u32,
            text: text.clone(),
        }),
        Delta::Thinking { index, text } => Kind::Thinking(pb::TextDelta {
            index: *index as u32,
            text: text.clone(),
        }),
        Delta::ToolCallArgs { index, json } => Kind::ToolArgs(pb::ToolArgs {
            index: *index as u32,
            json: json.clone(),
        }),
        Delta::BlockData {
            index,
            provider_data,
        } => Kind::BlockData(pb::BlockData {
            index: *index as u32,
            provider_data_json: json_string(provider_data),
        }),
        Delta::BlockStop { index } => Kind::BlockStop(pb::BlockStop {
            index: *index as u32,
        }),
        Delta::Usage(u) => Kind::Usage(usage_to_proto(u)),
        Delta::Stop(s) => Kind::StopReason(stop_reason_str(s)),
    };
    pb::ModelDelta { kind: Some(kind) }
}

#[cfg_attr(not(test), allow(dead_code))] // decode side, for resume
fn delta_from_proto(d: pb::ModelDelta) -> Option<Delta> {
    use pb::model_delta::Kind;
    Some(match d.kind? {
        Kind::BlockStart(b) => Delta::BlockStart {
            index: b.index as usize,
            block: block_from_proto(b.block?)?,
        },
        Kind::Text(t) => Delta::Text {
            index: t.index as usize,
            text: t.text,
        },
        Kind::Thinking(t) => Delta::Thinking {
            index: t.index as usize,
            text: t.text,
        },
        Kind::ToolArgs(a) => Delta::ToolCallArgs {
            index: a.index as usize,
            json: a.json,
        },
        Kind::BlockData(b) => Delta::BlockData {
            index: b.index as usize,
            provider_data: json_value(&b.provider_data_json),
        },
        Kind::BlockStop(b) => Delta::BlockStop {
            index: b.index as usize,
        },
        Kind::Usage(u) => Delta::Usage(usage_from_proto(u)),
        Kind::StopReason(s) => Delta::Stop(stop_reason_from(&s)),
    })
}

pub fn to_proto(ev: &Event) -> pb::Event {
    use pb::event::Kind;
    let kind = match ev {
        Event::UserMessage { text } => Kind::UserMessage(pb::UserMessage { text: text.clone() }),
        Event::UserInterrupt => Kind::UserInterrupt(pb::Interrupt {}),
        Event::AssistantMessage {
            content,
            stop_reason,
            usage,
        } => Kind::AssistantMessage(pb::AssistantMessage {
            content: content.iter().filter_map(block_to_proto).collect(),
            stop_reason: stop_reason_str(stop_reason),
            usage: Some(usage_to_proto(usage)),
        }),
        Event::ModelDelta { delta } => Kind::ModelDelta(delta_to_proto(delta)),
        Event::ModelCancelled => Kind::ModelCancelled(pb::ModelCancelled {}),
        Event::Error { message, retryable } => Kind::Error(pb::Error {
            message: message.clone(),
            retryable: *retryable,
        }),
        Event::ToolResult {
            call_id,
            name,
            content,
            is_error,
            meta,
        } => Kind::ToolResult(pb::ToolResult {
            call_id: call_id.clone(),
            name: name.clone(),
            content: content.clone(),
            is_error: *is_error,
            meta_json: json_string(meta),
        }),
        Event::TurnEnded { reason } => Kind::TurnEnded(pb::TurnEnded {
            reason: match reason {
                TurnEndReason::Completed => pb::TurnEndReason::Completed,
                TurnEndReason::Aborted => pb::TurnEndReason::Aborted,
                TurnEndReason::Failed => pb::TurnEndReason::Failed,
            } as i32,
        }),
    };
    pb::Event {
        ts_micros: now_micros(),
        kind: Some(kind),
    }
}

/// Inverse of [`to_proto`]; what a stored-event `Session` will use to resume.
#[cfg_attr(not(test), allow(dead_code))]
pub fn from_proto(ev: pb::Event) -> Option<Event> {
    use pb::event::Kind;
    Some(match ev.kind? {
        Kind::UserMessage(m) => Event::UserMessage { text: m.text },
        Kind::UserInterrupt(_) => Event::UserInterrupt,
        Kind::AssistantMessage(m) => Event::AssistantMessage {
            content: m.content.into_iter().filter_map(block_from_proto).collect(),
            stop_reason: stop_reason_from(&m.stop_reason),
            usage: usage_from_proto(m.usage.unwrap_or_default()),
        },
        Kind::ModelDelta(d) => Event::ModelDelta {
            delta: delta_from_proto(d)?,
        },
        Kind::ModelCancelled(_) => Event::ModelCancelled,
        Kind::Error(e) => Event::Error {
            message: e.message,
            retryable: e.retryable,
        },
        Kind::ToolResult(r) => Event::ToolResult {
            call_id: r.call_id,
            name: r.name,
            content: r.content,
            is_error: r.is_error,
            meta: json_value(&r.meta_json),
        },
        Kind::TurnEnded(t) => Event::TurnEnded {
            reason: match pb::TurnEndReason::try_from(t.reason).ok()? {
                pb::TurnEndReason::Completed => TurnEndReason::Completed,
                pb::TurnEndReason::Aborted => TurnEndReason::Aborted,
                pb::TurnEndReason::Failed => TurnEndReason::Failed,
                pb::TurnEndReason::Unspecified => return None,
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, AgentConfig};
    use crate::provider::mock::{MockProvider, Script};
    use crate::tools::Registry;
    use pb::dirigent_client::DirigentClient;
    use serde_json::json;
    use std::time::Duration;
    use tokio_stream::wrappers::ReceiverStream;

    #[test]
    fn every_event_round_trips_through_proto() {
        let events = vec![
            Event::UserMessage { text: "hi".into() },
            Event::UserInterrupt,
            Event::AssistantMessage {
                content: vec![
                    ContentBlock::Thinking {
                        text: "hmm".into(),
                        provider_data: json!({"signature": "s"}),
                    },
                    ContentBlock::Text { text: "ok".into() },
                    ContentBlock::ToolCall {
                        id: "c1".into(),
                        name: "Bash".into(),
                        input: json!({"command": "ls"}),
                    },
                ],
                stop_reason: StopReason::ToolUse,
                usage: Usage {
                    input: 1,
                    output: 2,
                    cache_read: 3,
                    cache_write: 4,
                },
            },
            Event::AssistantMessage {
                content: vec![],
                stop_reason: StopReason::Other {
                    raw: "weird".into(),
                },
                usage: Usage::default(),
            },
            Event::ModelDelta {
                delta: Delta::BlockStart {
                    index: 1,
                    block: ContentBlock::ToolCall {
                        id: "c1".into(),
                        name: "Bash".into(),
                        input: Value::Null,
                    },
                },
            },
            Event::ModelDelta {
                delta: Delta::Text {
                    index: 0,
                    text: "he".into(),
                },
            },
            Event::ModelDelta {
                delta: Delta::Thinking {
                    index: 0,
                    text: "hm".into(),
                },
            },
            Event::ModelDelta {
                delta: Delta::ToolCallArgs {
                    index: 1,
                    json: "{\"comm".into(),
                },
            },
            Event::ModelDelta {
                delta: Delta::BlockData {
                    index: 0,
                    provider_data: json!({"signature": "s"}),
                },
            },
            Event::ModelDelta {
                delta: Delta::BlockStop { index: 1 },
            },
            Event::ModelDelta {
                delta: Delta::Usage(Usage {
                    input: 5,
                    output: 6,
                    cache_read: 7,
                    cache_write: 8,
                }),
            },
            Event::ModelDelta {
                delta: Delta::Stop(StopReason::ToolUse),
            },
            Event::ModelCancelled,
            Event::Error {
                message: "boom".into(),
                retryable: true,
            },
            Event::ToolResult {
                call_id: "c1".into(),
                name: "Bash".into(),
                content: "out".into(),
                is_error: false,
                meta: json!({"exit_code": 0}),
            },
            Event::ToolResult {
                call_id: "c2".into(),
                name: "Read".into(),
                content: "x".into(),
                is_error: true,
                meta: Value::Null,
            },
            Event::TurnEnded {
                reason: TurnEndReason::Aborted,
            },
        ];
        for ev in events {
            let pb = to_proto(&ev);
            assert!(pb.ts_micros > 0);
            assert_eq!(from_proto(pb), Some(ev));
        }
    }

    /// Server + agent with a mock provider, sessions handled one at a time.
    async fn serve(provider: Arc<MockProvider>) -> (SocketAddr, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let workdir: &'static std::path::Path =
            Box::leak(dir.path().canonicalize().unwrap().into_boxed_path());
        let mut server = GrpcServer::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = server.addr();
        let agent = Agent {
            provider,
            cfg: AgentConfig {
                model: "mock",
                system: "sys",
                effort: None,
            },
            tools: Arc::new(Registry::default_set()),
            workdir,
        };
        tokio::spawn(async move {
            while let Some(mut s) = server.accept().await {
                agent.run(&mut s).await;
            }
        });
        (addr, dir)
    }

    struct Client {
        input: mpsc::Sender<pb::Input>,
        events: Streaming<pb::Event>,
    }

    async fn connect(addr: SocketAddr) -> Result<Client, Status> {
        let mut client = DirigentClient::connect(format!("http://{addr}"))
            .await
            .unwrap();
        let (input, rx) = mpsc::channel(8);
        let events = client.session(ReceiverStream::new(rx)).await?.into_inner();
        Ok(Client { input, events })
    }

    impl Client {
        async fn say(&self, text: &str) {
            self.input
                .send(pb::Input {
                    kind: Some(pb::input::Kind::Message(pb::UserMessage {
                        text: text.into(),
                    })),
                })
                .await
                .unwrap();
        }

        /// Next non-delta event, rendered as the proto oneof variant name.
        async fn next(&mut self) -> String {
            let ev = loop {
                let ev = tokio::time::timeout(Duration::from_secs(5), self.events.message())
                    .await
                    .expect("timed out")
                    .unwrap()
                    .expect("stream ended");
                if !matches!(ev.kind, Some(pb::event::Kind::ModelDelta(_))) {
                    break ev;
                }
            };
            match ev.kind.unwrap() {
                pb::event::Kind::UserMessage(m) => format!("user_message:{}", m.text),
                pb::event::Kind::AssistantMessage(m) => {
                    format!("assistant_message:{}", m.content.len())
                }
                pb::event::Kind::TurnEnded(t) => format!("turn_ended:{}", t.reason),
                other => format!("{other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn one_session_at_a_time_and_fresh_history_per_session() {
        let provider = Arc::new(MockProvider::default());
        provider.reset([
            Script::text("one", Duration::ZERO),
            Script::text("two", Duration::ZERO),
        ]);
        let (addr, _dir) = serve(provider.clone()).await;

        let mut first = connect(addr).await.unwrap();
        first.say("hi").await;
        assert_eq!(first.next().await, "user_message:hi");
        assert_eq!(first.next().await, "assistant_message:1");
        assert_eq!(
            first.next().await,
            format!("turn_ended:{}", pb::TurnEndReason::Completed as i32)
        );

        let refused = connect(addr).await.err().expect("second client refused");
        assert_eq!(refused.code(), tonic::Code::FailedPrecondition);

        // Closing the first client ends its session; the next one starts fresh.
        drop(first.input);
        assert!(
            tokio::time::timeout(Duration::from_secs(5), first.events.message())
                .await
                .unwrap()
                .unwrap()
                .is_none()
        );
        let mut second = loop {
            match connect(addr).await {
                Ok(c) => break c,
                Err(s) if s.code() == tonic::Code::FailedPrecondition => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                Err(s) => panic!("{s}"),
            }
        };
        second.say("again").await;
        assert_eq!(second.next().await, "user_message:again");
        assert_eq!(second.next().await, "assistant_message:1");
        let reqs = provider.requests();
        assert_eq!(reqs.len(), 2);
        assert_eq!(
            reqs[1].messages.len(),
            1,
            "history does not leak between sessions"
        );
    }
}
