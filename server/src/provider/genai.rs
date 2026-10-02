use std::collections::HashMap;
use std::time::Duration;

use ::genai::chat::{
    ChatMessage, ChatOptions, ChatRequest, ChatStreamEvent, ContentPart, MessageContent,
    ReasoningEffort, StopReason as GStop, StreamEnd, Tool, ToolCall, ToolResponse, Usage as GUsage,
};
use ::genai::resolver::{AuthData, Endpoint};
use ::genai::{Client, ServiceTarget};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};

use super::{
    ContentBlock, Delta, DeltaStream, Effort, Message, Provider, ProviderError, Request, Role,
    StopReason, Usage,
};

pub struct GenaiProvider {
    client: Client,
}

impl GenaiProvider {
    /// `base_url`, when set, replaces the provider's endpoint for every call
    /// (Anthropic default: `https://api.anthropic.com/v1/`; keep the `/v1/`).
    pub fn new(api_key: String, base_url: Option<String>) -> anyhow::Result<Self> {
        let mut builder = Client::builder()
            .with_auth_resolver_fn(move |_| Ok(Some(AuthData::from_single(api_key.clone()))));
        if let Some(url) = base_url {
            builder = builder.with_service_target_resolver_fn(move |mut t: ServiceTarget| {
                t.endpoint = Endpoint::from_owned(url.clone());
                Ok(t)
            });
        }
        Ok(GenaiProvider {
            client: builder.build()?,
        })
    }
}

#[async_trait]
impl Provider for GenaiProvider {
    fn name(&self) -> &'static str {
        "genai"
    }

    async fn stream(&self, req: &Request) -> Result<DeltaStream, ProviderError> {
        let (chat_req, opts) = to_chat_request(req);
        let res = self
            .client
            .exec_chat_stream(req.model.as_str(), chat_req, Some(&opts))
            .await
            .map_err(classify_error)?;
        let mut st = MapState::default();
        let stream = res.stream.flat_map(move |item| {
            let out: Vec<Result<Delta, ProviderError>> = match item {
                Ok(ev) => st.map(ev).into_iter().map(Ok).collect(),
                Err(e) => vec![Err(classify_error(e))],
            };
            futures::stream::iter(out)
        });
        Ok(stream.boxed())
    }
}

// ---------------------------------------------------------------------------
// Request mapping
// ---------------------------------------------------------------------------

pub fn to_chat_request(req: &Request) -> (ChatRequest, ChatOptions) {
    let mut messages: Vec<ChatMessage> = Vec::new();
    for msg in &req.messages {
        messages.extend(to_chat_messages(msg));
    }
    let mut chat = ChatRequest::new(messages);
    if let Some(system) = &req.system {
        chat = chat.with_system(system.clone());
    }
    if !req.tools.is_empty() {
        chat = chat.with_tools(req.tools.iter().map(|t| {
            Tool::new(t.name.clone())
                .with_description(t.description.clone())
                .with_schema(t.input_schema.clone())
        }));
    }
    let opts = ChatOptions::default()
        .with_capture_usage(true)
        .with_capture_content(true)
        .with_capture_tool_calls(true)
        .with_capture_reasoning_content(true)
        .with_reasoning_effort(match req.effort {
            None => ReasoningEffort::Zero,
            Some(Effort::Low) => ReasoningEffort::Low,
            Some(Effort::Medium) => ReasoningEffort::Medium,
            Some(Effort::High) => ReasoningEffort::High,
            Some(Effort::Max) => ReasoningEffort::Max,
        });
    (chat, opts)
}

/// One neutral message becomes one genai message, except that tool results
/// inside a user message are split into a preceding `Tool`-role message
/// (genai's shape; the Anthropic adapter folds it back into a user turn).
fn to_chat_messages(msg: &Message) -> Vec<ChatMessage> {
    let mut out = Vec::new();
    match msg.role {
        Role::User => {
            let mut results = Vec::new();
            let mut parts = Vec::new();
            for block in &msg.content {
                match block {
                    ContentBlock::ToolResult {
                        call_id,
                        content,
                        is_error,
                    } => {
                        let content = if *is_error {
                            format!("[error] {content}")
                        } else {
                            content.clone()
                        };
                        results.push(ToolResponse::new(call_id.clone(), content));
                    }
                    ContentBlock::Text { text } => parts.push(ContentPart::Text(text.clone())),
                    // Thinking / ToolCall never appear in user messages.
                    _ => {}
                }
            }
            if !results.is_empty() {
                out.push(ChatMessage::tool(MessageContent::from_tool_responses(
                    results,
                )));
            }
            if !parts.is_empty() {
                out.push(ChatMessage::user(MessageContent::from_parts(parts)));
            }
        }
        Role::Assistant => {
            let mut parts = Vec::new();
            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => parts.push(ContentPart::Text(text.clone())),
                    ContentBlock::Thinking {
                        text,
                        provider_data,
                    } => {
                        // Anthropic accepts a thinking block only with its
                        // signature; an unsigned one is dropped on purpose.
                        if let Some(sig) = provider_data.get("signature").and_then(Value::as_str) {
                            parts.push(ContentPart::ReasoningContent(text.clone()));
                            parts.push(ContentPart::ThoughtSignature(sig.to_string()));
                        }
                    }
                    ContentBlock::ToolCall { id, name, input } => {
                        parts.push(ContentPart::ToolCall(ToolCall {
                            call_id: id.clone(),
                            fn_name: name.clone(),
                            fn_arguments: input.clone(),
                            thought_signatures: None,
                        }));
                    }
                    ContentBlock::ToolResult { .. } => {}
                }
            }
            if !parts.is_empty() {
                out.push(ChatMessage::assistant(MessageContent::from_parts(parts)));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Stream mapping
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Open {
    Text(usize),
    Thinking(usize),
    Tool(usize),
}

/// Turns genai's flat chunk events into our indexed block deltas.
#[derive(Debug, Default)]
pub struct MapState {
    next_index: usize,
    open: Option<Open>,
    tool_index: HashMap<String, usize>,
}

impl MapState {
    pub fn map(&mut self, ev: ChatStreamEvent) -> Vec<Delta> {
        let mut out = Vec::new();
        match ev {
            ChatStreamEvent::Start | ChatStreamEvent::Heartbeat => {}
            ChatStreamEvent::Chunk(c) => {
                let index = match self.open {
                    Some(Open::Text(i)) => i,
                    _ => self.open_block(
                        &mut out,
                        ContentBlock::Text {
                            text: String::new(),
                        },
                        Open::Text,
                    ),
                };
                out.push(Delta::Text {
                    index,
                    text: c.content,
                });
            }
            ChatStreamEvent::ReasoningChunk(c) => {
                let index = match self.open {
                    Some(Open::Thinking(i)) => i,
                    _ => self.open_block(
                        &mut out,
                        ContentBlock::Thinking {
                            text: String::new(),
                            provider_data: Value::Null,
                        },
                        Open::Thinking,
                    ),
                };
                out.push(Delta::Thinking {
                    index,
                    text: c.content,
                });
            }
            ChatStreamEvent::ThoughtSignatureChunk(c) => {
                let index = match self.open {
                    Some(Open::Thinking(i)) => i,
                    _ => self.open_block(
                        &mut out,
                        ContentBlock::Thinking {
                            text: String::new(),
                            provider_data: Value::Null,
                        },
                        Open::Thinking,
                    ),
                };
                out.push(Delta::BlockData {
                    index,
                    provider_data: json!({ "signature": c.content }),
                });
                // The signature is the last thing in a thinking block.
                self.close(&mut out);
            }
            ChatStreamEvent::ToolCallChunk(tc) => {
                let tc = tc.tool_call;
                let index = match self.tool_index.get(&tc.call_id) {
                    Some(i) => *i,
                    None => {
                        let i = self.open_block(
                            &mut out,
                            ContentBlock::ToolCall {
                                id: tc.call_id.clone(),
                                name: tc.fn_name.clone(),
                                input: Value::Null,
                            },
                            Open::Tool,
                        );
                        self.tool_index.insert(tc.call_id.clone(), i);
                        i
                    }
                };
                let json = tool_args_json(&tc.fn_arguments);
                if !json.is_empty() {
                    out.push(Delta::ToolCallArgs { index, json });
                }
            }
            ChatStreamEvent::End(end) => {
                self.close(&mut out);
                self.finish(&mut out, end);
            }
        }
        out
    }

    fn open_block(
        &mut self,
        out: &mut Vec<Delta>,
        block: ContentBlock,
        mk: fn(usize) -> Open,
    ) -> usize {
        self.close(out);
        let index = self.next_index;
        self.next_index += 1;
        out.push(Delta::BlockStart { index, block });
        self.open = Some(mk(index));
        index
    }

    fn close(&mut self, out: &mut Vec<Delta>) {
        if let Some(open) = self.open.take() {
            let index = match open {
                Open::Text(i) | Open::Thinking(i) | Open::Tool(i) => i,
            };
            out.push(Delta::BlockStop { index });
        }
    }

    fn finish(&mut self, out: &mut Vec<Delta>, end: StreamEnd) {
        // Final, parsed tool arguments win over the streamed buffer.
        if let Some(content) = &end.captured_content {
            for tc in content.tool_calls() {
                if let Some(&index) = self.tool_index.get(&tc.call_id) {
                    let json = tool_args_json(&tc.fn_arguments);
                    if !json.is_empty() {
                        out.push(Delta::ToolCallArgs { index, json });
                    }
                }
            }
        }
        if let Some(usage) = &end.captured_usage {
            out.push(Delta::Usage(to_usage(usage)));
        }
        let stop = match end.captured_stop_reason {
            Some(s) => to_stop_reason(s),
            None if !self.tool_index.is_empty() => StopReason::ToolUse,
            None => StopReason::EndTurn,
        };
        out.push(Delta::Stop(stop));
    }
}

/// genai streams tool arguments as `Value::String(<accumulated json>)` and
/// finalizes them as a parsed object.
fn tool_args_json(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// genai adds cached tokens into `prompt_tokens`; we keep them apart.
pub fn to_usage(u: &GUsage) -> Usage {
    let nz = |v: Option<i32>| v.unwrap_or(0).max(0) as u64;
    let details = u.prompt_tokens_details.as_ref();
    let cache_read = nz(details.and_then(|d| d.cached_tokens));
    let cache_write = nz(details.and_then(|d| d.cache_creation_tokens));
    let prompt = nz(u.prompt_tokens);
    Usage {
        input: prompt.saturating_sub(cache_read + cache_write),
        output: nz(u.completion_tokens),
        cache_read,
        cache_write,
    }
}

pub fn to_stop_reason(s: GStop) -> StopReason {
    match s {
        GStop::Completed(_) => StopReason::EndTurn,
        GStop::MaxTokens(_) => StopReason::MaxTokens,
        GStop::ToolCall(_) => StopReason::ToolUse,
        GStop::ContentFilter(_) => StopReason::Refusal,
        GStop::StopSequence(raw) => StopReason::Other { raw },
        GStop::Other(raw) => match raw.as_str() {
            "model_context_window_exceeded" => StopReason::ContextOverflow,
            "refusal" => StopReason::Refusal,
            _ => StopReason::Other { raw },
        },
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

pub fn classify_error(e: ::genai::Error) -> ProviderError {
    let status = e.status().map(|s| s.as_u16());
    let text = e.to_string();
    let lower = text.to_ascii_lowercase();
    match status {
        Some(429) => ProviderError::RateLimited {
            retry_after: retry_after(&e),
        },
        Some(413) => ProviderError::ContextOverflow,
        Some(400) if lower.contains("prompt is too long") || lower.contains("context window") => {
            ProviderError::ContextOverflow
        }
        Some(s) => ProviderError::Api {
            status: s,
            message: text,
            retryable: s >= 500 || lower.contains("overloaded"),
        },
        None => match e {
            ::genai::Error::StreamParse { .. } | ::genai::Error::WebStream { .. } => {
                ProviderError::Stream(text)
            }
            _ => ProviderError::Transport(text),
        },
    }
}

fn retry_after(e: &::genai::Error) -> Option<Duration> {
    let headers = match e {
        ::genai::Error::HttpError { headers, .. } => headers.as_ref(),
        ::genai::Error::WebModelCall { webc_error, .. }
        | ::genai::Error::WebAdapterCall { webc_error, .. } => match webc_error {
            ::genai::webc::Error::ResponseFailedStatus { headers, .. } => headers.as_ref(),
            _ => return None,
        },
        _ => return None,
    };
    headers
        .get("retry-after")?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Accumulator, ToolSpec};
    use ::genai::chat::{ChatRole, PromptTokensDetails, StreamChunk, ToolChunk};
    use ::genai::webc;
    use http::header::{HeaderMap, HeaderValue};
    use http::StatusCode;

    fn end(stop: Option<GStop>, usage: Option<GUsage>) -> ChatStreamEvent {
        ChatStreamEvent::End(StreamEnd {
            captured_usage: usage,
            captured_stop_reason: stop,
            captured_content: None,
            captured_reasoning_content: None,
            captured_response_id: None,
        })
    }

    fn chunk(s: &str) -> StreamChunk {
        StreamChunk {
            content: s.to_string(),
        }
    }

    #[test]
    fn request_maps_system_tools_and_reasoning() {
        let req = Request {
            model: "claude-sonnet-5".into(),
            system: Some("be terse".into()),
            messages: vec![Message::user("hi")],
            tools: vec![ToolSpec {
                name: "Read".into(),
                description: "read a file".into(),
                input_schema: json!({"type": "object"}),
            }],
            effort: Some(Effort::High),
        };
        let (chat, opts) = to_chat_request(&req);
        assert_eq!(chat.system.as_deref(), Some("be terse"));
        let tools = chat.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name.as_str(), "Read");
        assert_eq!(tools[0].description.as_deref(), Some("read a file"));
        assert_eq!(opts.max_tokens, None); // genai fills in the model maximum
        assert!(matches!(opts.reasoning_effort, Some(ReasoningEffort::High)));
        assert_eq!(opts.capture_reasoning_content, Some(true));
        assert_eq!(chat.messages.len(), 1);
        assert!(matches!(chat.messages[0].role, ChatRole::User));

        let (_, opts) = to_chat_request(&Request {
            effort: None,
            ..req
        });
        assert!(matches!(opts.reasoning_effort, Some(ReasoningEffort::Zero)));
    }

    #[test]
    fn request_maps_tool_turn_and_signed_thinking() {
        let req = Request {
            model: "claude-sonnet-5".into(),
            system: None,
            messages: vec![
                Message::user("run it"),
                Message::assistant(vec![
                    ContentBlock::Thinking {
                        text: "plan".into(),
                        provider_data: json!({"signature": "SIG"}),
                    },
                    ContentBlock::Thinking {
                        text: "unsigned, must be dropped".into(),
                        provider_data: Value::Null,
                    },
                    ContentBlock::ToolCall {
                        id: "c1".into(),
                        name: "Bash".into(),
                        input: json!({"command": "ls"}),
                    },
                ]),
                Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::ToolResult {
                            call_id: "c1".into(),
                            content: "boom".into(),
                            is_error: true,
                        },
                        ContentBlock::Text {
                            text: "and then?".into(),
                        },
                    ],
                },
            ],
            tools: vec![],
            effort: None,
        };
        let (chat, _) = to_chat_request(&req);
        let roles: Vec<_> = chat.messages.iter().map(|m| m.role.clone()).collect();
        assert!(matches!(
            roles.as_slice(),
            [
                ChatRole::User,
                ChatRole::Assistant,
                ChatRole::Tool,
                ChatRole::User
            ]
        ));

        let asst = &chat.messages[1].content;
        assert_eq!(asst.reasoning_contents(), vec!["plan"]);
        assert_eq!(asst.thought_signatures(), vec!["SIG"]);
        let calls = asst.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].call_id, "c1");
        assert_eq!(calls[0].fn_arguments, json!({"command": "ls"}));

        let results = chat.messages[2].content.tool_responses();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].call_id, "c1");
        assert_eq!(results[0].content, "[error] boom");
        assert_eq!(chat.messages[3].content.first_text(), Some("and then?"));
    }

    #[test]
    fn stream_text_only() {
        let mut st = MapState::default();
        let mut deltas = Vec::new();
        deltas.extend(st.map(ChatStreamEvent::Start));
        deltas.extend(st.map(ChatStreamEvent::Chunk(chunk("Hel"))));
        deltas.extend(st.map(ChatStreamEvent::Chunk(chunk("lo"))));
        let usage = GUsage {
            prompt_tokens: Some(100),
            prompt_tokens_details: Some(PromptTokensDetails {
                cached_tokens: Some(60),
                cache_creation_tokens: Some(10),
                ..Default::default()
            }),
            completion_tokens: Some(7),
            ..Default::default()
        };
        deltas.extend(st.map(end(Some(GStop::Completed("end_turn".into())), Some(usage))));
        assert_eq!(
            deltas,
            vec![
                Delta::BlockStart {
                    index: 0,
                    block: ContentBlock::Text {
                        text: String::new()
                    }
                },
                Delta::Text {
                    index: 0,
                    text: "Hel".into()
                },
                Delta::Text {
                    index: 0,
                    text: "lo".into()
                },
                Delta::BlockStop { index: 0 },
                Delta::Usage(Usage {
                    input: 30,
                    output: 7,
                    cache_read: 60,
                    cache_write: 10
                }),
                Delta::Stop(StopReason::EndTurn),
            ]
        );
        let mut acc = Accumulator::default();
        deltas.into_iter().for_each(|d| acc.push(d));
        assert_eq!(acc.finish().message.text(), "Hello");
    }

    #[test]
    fn stream_thinking_then_tool_call() {
        let mut st = MapState::default();
        let mut deltas = Vec::new();
        deltas.extend(st.map(ChatStreamEvent::ReasoningChunk(chunk("think"))));
        deltas.extend(st.map(ChatStreamEvent::ReasoningChunk(chunk("ing"))));
        deltas.extend(st.map(ChatStreamEvent::ThoughtSignatureChunk(chunk("SIG"))));
        let tc = |args: Value| {
            ChatStreamEvent::ToolCallChunk(ToolChunk {
                tool_call: ToolCall {
                    call_id: "c1".into(),
                    fn_name: "Read".into(),
                    fn_arguments: args,
                    thought_signatures: None,
                },
            })
        };
        deltas.extend(st.map(tc(Value::String(String::new()))));
        deltas.extend(st.map(tc(Value::String("{\"file".into()))));
        deltas.extend(st.map(tc(Value::String("{\"file_path\": \"/x\"}".into()))));
        let captured = MessageContent::from_tool_calls(vec![ToolCall {
            call_id: "c1".into(),
            fn_name: "Read".into(),
            fn_arguments: json!({"file_path": "/x"}),
            thought_signatures: None,
        }]);
        deltas.extend(st.map(ChatStreamEvent::End(StreamEnd {
            captured_usage: None,
            captured_stop_reason: Some(GStop::ToolCall("tool_use".into())),
            captured_content: Some(captured),
            captured_reasoning_content: None,
            captured_response_id: None,
        })));

        let mut acc = Accumulator::default();
        deltas.iter().cloned().for_each(|d| acc.push(d));
        let c = acc.finish();
        assert_eq!(c.stop_reason, StopReason::ToolUse);
        assert_eq!(
            c.message.content,
            vec![
                ContentBlock::Thinking {
                    text: "thinking".into(),
                    provider_data: json!({"signature": "SIG"}),
                },
                ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "Read".into(),
                    input: json!({"file_path": "/x"}),
                },
            ]
        );
        // Thinking block was closed by its signature, before the tool call opened.
        let stops: Vec<_> = deltas
            .iter()
            .filter(|d| matches!(d, Delta::BlockStop { .. }))
            .collect();
        assert_eq!(stops.len(), 2);
    }

    #[test]
    fn stop_reason_mapping() {
        assert_eq!(
            to_stop_reason(GStop::Other("model_context_window_exceeded".into())),
            StopReason::ContextOverflow
        );
        assert_eq!(
            to_stop_reason(GStop::Other("refusal".into())),
            StopReason::Refusal
        );
        assert_eq!(
            to_stop_reason(GStop::MaxTokens("max_tokens".into())),
            StopReason::MaxTokens
        );
        assert_eq!(
            to_stop_reason(GStop::Other("weird".into())),
            StopReason::Other {
                raw: "weird".into()
            }
        );
    }

    fn http(status: u16, body: &str, headers: HeaderMap) -> ::genai::Error {
        ::genai::Error::HttpError {
            status: StatusCode::from_u16(status).unwrap(),
            canonical_reason: String::new(),
            body: body.to_string(),
            headers: Box::new(headers),
        }
    }

    #[test]
    fn error_classification() {
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("7"));
        assert!(matches!(
            classify_error(http(429, "slow down", h)),
            ProviderError::RateLimited {
                retry_after: Some(d)
            } if d == Duration::from_secs(7)
        ));
        assert!(matches!(
            classify_error(http(413, "", HeaderMap::new())),
            ProviderError::ContextOverflow
        ));
        assert!(matches!(
            classify_error(http(
                400,
                r#"{"error":{"message":"prompt is too long: 1000001 tokens"}}"#,
                HeaderMap::new()
            )),
            ProviderError::ContextOverflow
        ));
        assert!(matches!(
            classify_error(http(400, "bad request", HeaderMap::new())),
            ProviderError::Api {
                status: 400,
                retryable: false,
                ..
            }
        ));
        assert!(matches!(
            classify_error(http(529, "overloaded_error", HeaderMap::new())),
            ProviderError::Api {
                status: 529,
                retryable: true,
                ..
            }
        ));
        // Status carried inside a webc error is honoured too.
        let inner = webc::Error::ResponseFailedStatus {
            status: StatusCode::SERVICE_UNAVAILABLE,
            body: "down".into(),
            headers: Box::new(HeaderMap::new()),
        };
        let e = ::genai::Error::WebModelCall {
            model_iden: ::genai::ModelIden::new(::genai::adapter::AdapterKind::Anthropic, "m"),
            webc_error: inner,
        };
        assert!(matches!(
            classify_error(e),
            ProviderError::Api {
                status: 503,
                retryable: true,
                ..
            }
        ));
    }
}
