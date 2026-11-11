use std::collections::HashMap;
use std::time::Duration;

use ::genai::chat::{
    CacheControl, ChatMessage, ChatOptions, ChatRequest, ChatStreamEvent, ContentPart,
    MessageContent, ReasoningEffort, StopReason as GStop, StreamEnd, Tool, ToolCall, ToolResponse,
    Usage as GUsage,
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
use crate::session::control::{AnthropicKey, PlanTokens};

pub struct GenaiProvider {
    client: Client,
    /// OpenAI's Responses API on a ChatGPT plan, with the lease's access tokens; none,
    /// Anthropic's.
    plan: Option<PlanTokens>,
    /// Anthropic's API key, the lease's; none, a ChatGPT plan's.
    key: Option<AnthropicKey>,
}

impl GenaiProvider {
    /// Anthropic's API with `key`, each call taking the latest. `base_url`, when set, replaces
    /// its endpoint for every call (by default `https://api.anthropic.com/v1/`; keep the `/v1/`).
    pub fn anthropic(key: AnthropicKey, base_url: Option<String>) -> anyhow::Result<Self> {
        let current = key.clone();
        let mut builder = Client::builder()
            .with_auth_resolver_fn(move |_| Ok(current.current().ok().map(AuthData::from_single)));
        if let Some(url) = base_url {
            builder = builder.with_service_target_resolver_fn(move |mut t: ServiceTarget| {
                t.endpoint = Endpoint::from_owned(url.clone());
                Ok(t)
            });
        }
        Ok(GenaiProvider {
            client: builder.build()?,
            plan: None,
            key: Some(key),
        })
    }

    /// OpenAI's Responses API at `base_url` (`https://api.openai.com/v1/`; keep the `/v1/`), paid
    /// with the ChatGPT plan whose access tokens `plan` has: each call takes the latest.
    pub fn chatgpt(plan: PlanTokens, base_url: String) -> anyhow::Result<Self> {
        let tokens = plan.clone();
        let client = Client::builder()
            .with_auth_resolver_fn(move |_| Ok(tokens.current().ok().map(AuthData::from_single)))
            .with_service_target_resolver_fn(move |mut t: ServiceTarget| {
                t.endpoint = Endpoint::from_owned(base_url.clone());
                Ok(t)
            })
            .build()?;
        Ok(GenaiProvider {
            client,
            plan: Some(plan),
            key: None,
        })
    }

    async fn open(&self, req: &Request) -> Result<DeltaStream, ProviderError> {
        let openai = self.plan.is_some();
        let (chat_req, opts) = to_chat_request(req, openai);
        // the Responses adapter, whatever the model's name
        let model = match openai {
            true => format!("openai_resp::{}", req.model),
            false => req.model.clone(),
        };
        let res = self
            .client
            .exec_chat_stream(model.as_str(), chat_req, Some(&opts))
            .await
            .map_err(classify_error)?;
        let mut st = MapState::default();
        let stream = res.stream.flat_map(move |item| {
            let out: Vec<Result<Delta, ProviderError>> = match item {
                Ok(ChatStreamEvent::End(end)) if st.ended_empty(&end) => {
                    vec![Err(ProviderError::Stream(EMPTY_END.into()))]
                }
                Ok(ev) => st.map(ev).into_iter().map(Ok).collect(),
                Err(e) => vec![Err(classify_error(e))],
            };
            futures::stream::iter(out)
        });
        Ok(stream.boxed())
    }
}

/// Why a ChatGPT plan's call can't be made, as the turn fails with it.
fn unavailable(why: String) -> ProviderError {
    ProviderError::Api {
        status: 401,
        message: format!("the ChatGPT plan: {why}"),
        retryable: false,
    }
}

/// Why an Anthropic session's call can't be made: it has no key.
fn no_key(why: String) -> ProviderError {
    ProviderError::Api {
        status: 401,
        message: why,
        retryable: false,
    }
}

fn refused(e: &ProviderError) -> bool {
    matches!(e, ProviderError::Api { status: 401, .. })
}

#[async_trait]
impl Provider for GenaiProvider {
    fn name(&self) -> &'static str {
        "genai"
    }

    /// On a ChatGPT plan, an access token OpenAI refuses (expired early, revoked) is renewed by
    /// control, and the call made once more with the new one.
    async fn stream(&self, req: &Request) -> Result<DeltaStream, ProviderError> {
        if let Some(key) = &self.key {
            key.current().map_err(no_key)?;
        }
        let Some(plan) = &self.plan else {
            return self.open(req).await;
        };
        let token = plan.current().map_err(unavailable)?;
        match self.open(req).await {
            Err(e) if refused(&e) => {}
            Err(e) => return Err(e),
            // a refusal comes as the stream's first item
            Ok(mut stream) => match stream.next().await {
                Some(Err(e)) if refused(&e) => {}
                first => return Ok(futures::stream::iter(first).chain(stream).boxed()),
            },
        }
        plan.renewed(&token).await.map_err(unavailable)?;
        self.open(req).await
    }
}

// ---------------------------------------------------------------------------
// Request mapping
// ---------------------------------------------------------------------------

/// Prompt cache breakpoints: the end of the static prefix (the tools, then the system prompt), the
/// last message, and the message the previous call ended on (the one before the last reply), so
/// the prefix that call cached is read back however many blocks were added since. For `openai`'s
/// Responses API, none (it caches prefixes itself), and the system prompt is its `instructions`
/// (a ChatGPT plan's calls may have no system message).
pub fn to_chat_request(req: &Request, openai: bool) -> (ChatRequest, ChatOptions) {
    if openai {
        let messages = req.messages.iter().flat_map(to_chat_messages).collect();
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
        return (chat, chat_options(req));
    }
    let last = req.messages.len().checked_sub(1);
    let previous = req
        .messages
        .iter()
        .rposition(|m| m.role == Role::Assistant)
        .and_then(|i| i.checked_sub(1));
    let mut messages: Vec<ChatMessage> = Vec::new();
    if let Some(system) = &req.system {
        messages.push(ChatMessage::system(system.clone()).with_options(CacheControl::Ephemeral));
    }
    for (i, msg) in req.messages.iter().enumerate() {
        let mut chat = to_chat_messages(msg);
        if Some(i) == last || Some(i) == previous {
            if let Some(m) = chat.last_mut() {
                m.options = Some(CacheControl::Ephemeral.into());
            }
        }
        messages.extend(chat);
    }
    let mut chat = ChatRequest::new(messages);
    if !req.tools.is_empty() {
        let last_tool = req.tools.len() - 1;
        chat = chat.with_tools(req.tools.iter().enumerate().map(|(i, t)| {
            let tool = Tool::new(t.name.clone())
                .with_description(t.description.clone())
                .with_schema(t.input_schema.clone());
            if req.system.is_none() && i == last_tool {
                tool.with_cache_control(CacheControl::Ephemeral)
            } else {
                tool
            }
        }));
    }
    (chat, chat_options(req))
}

fn chat_options(req: &Request) -> ChatOptions {
    ChatOptions::default()
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
        })
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
    /// The thinking blocks without a signature yet, in order.
    unsigned: Vec<usize>,
    /// Whether signatures came streamed (Anthropic's), not only at the end (OpenAI's).
    streamed_signatures: bool,
}

/// A stream that ended with nothing: no stop reason, usage or content. genai ends a Responses
/// stream so when the server's event isn't one it knows (an `error` one: a refused request), or
/// the connection closes early; either way not a reply.
const EMPTY_END: &str = "the model's response ended without a reply";

impl MapState {
    /// Whether `end` ends a stream that had nothing (see [`EMPTY_END`]).
    pub fn ended_empty(&self, end: &StreamEnd) -> bool {
        self.next_index == 0
            && end.captured_stop_reason.is_none()
            && end.captured_usage.is_none()
            && end.captured_content.as_ref().is_none_or(|c| c.is_empty())
    }

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
                    _ => {
                        let i = self.open_block(
                            &mut out,
                            ContentBlock::Thinking {
                                text: String::new(),
                                provider_data: Value::Null,
                            },
                            Open::Thinking,
                        );
                        self.unsigned.push(i);
                        i
                    }
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
                self.unsigned.retain(|&i| i != index);
                self.streamed_signatures = true;
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
        // OpenAI's reasoning comes encrypted at the end only (its items' `encrypted_content`):
        // the signatures of the thinking blocks summarizing it, in order, or of blocks of their
        // own, so the reasoning goes back with the next call (`store: false` keeps none)
        if !self.streamed_signatures {
            let signatures = end.captured_thought_signatures().unwrap_or_default();
            let mut unsigned = std::mem::take(&mut self.unsigned).into_iter();
            for signature in signatures {
                let index = match unsigned.next() {
                    Some(i) => i,
                    None => {
                        let i = self.open_block(
                            out,
                            ContentBlock::Thinking {
                                text: String::new(),
                                provider_data: Value::Null,
                            },
                            Open::Thinking,
                        );
                        self.close(out);
                        i
                    }
                };
                out.push(Delta::BlockData {
                    index,
                    provider_data: json!({ "signature": signature }),
                });
            }
        }
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
            // OpenAI's Responses API says `completed` of a reply of function calls too
            Some(GStop::Completed(_)) | None if !self.tool_index.is_empty() => StopReason::ToolUse,
            Some(s) => to_stop_reason(s),
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
    // A streaming call's HTTP error comes wrapped as a stream error.
    let e = match e {
        ::genai::Error::WebStream {
            model_iden,
            cause,
            error,
        } => match error.downcast::<::genai::Error>() {
            Ok(inner) => *inner,
            Err(error) => ::genai::Error::WebStream {
                model_iden,
                cause,
                error,
            },
        },
        e => e,
    };
    let status = e.status().map(|s| s.as_u16());
    let text = e.to_string();
    let lower = text.to_ascii_lowercase();
    // a ChatGPT plan's limit (or its app's) is reached: not a rate limit that passes in a while
    if lower.contains("subscription_sharing_usage_limit_exceeded") {
        return ProviderError::Api {
            status: status.unwrap_or(429),
            message:
                "the ChatGPT plan's usage limit is reached (see https://chatgpt.com/settings/usage)"
                    .into(),
            retryable: false,
        };
    }
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
    use crate::provider::{Accumulator, Completion, ToolSpec};
    use ::genai::chat::{ChatRole, PromptTokensDetails, StreamChunk, ToolChunk};
    use ::genai::webc;
    use http::header::{HeaderMap, HeaderValue};
    use http::StatusCode;
    use std::sync::Arc;

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
        let (chat, opts) = to_chat_request(&req, false);
        assert_eq!(
            chat.messages[0].content.joined_texts().as_deref(),
            Some("be terse")
        );
        let tools = chat.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name.as_str(), "Read");
        assert_eq!(tools[0].description.as_deref(), Some("read a file"));
        assert_eq!(opts.max_tokens, None); // genai fills in the model maximum
        assert!(matches!(opts.reasoning_effort, Some(ReasoningEffort::High)));
        assert_eq!(opts.capture_reasoning_content, Some(true));
        let roles: Vec<_> = chat.messages.iter().map(|m| m.role.clone()).collect();
        assert!(matches!(
            roles.as_slice(),
            [ChatRole::System, ChatRole::User]
        ));

        let (_, opts) = to_chat_request(
            &Request {
                effort: None,
                ..req
            },
            false,
        );
        assert!(matches!(opts.reasoning_effort, Some(ReasoningEffort::Zero)));
    }

    fn cached(chat: &ChatRequest) -> Vec<bool> {
        chat.messages
            .iter()
            .map(|m| {
                m.options
                    .as_ref()
                    .is_some_and(|o| o.cache_control == Some(CacheControl::Ephemeral))
            })
            .collect()
    }

    #[test]
    fn cache_breakpoints_end_the_system_prompt_the_last_call_and_this_one() {
        let call = |id: &str| ContentBlock::ToolCall {
            id: id.into(),
            name: "Bash".into(),
            input: json!({"command": "ls"}),
        };
        let result = |id: &str| Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                call_id: id.into(),
                content: "ok".into(),
                is_error: false,
            }],
        };
        let mut req = Request {
            model: "claude-sonnet-5".into(),
            system: Some("sys".into()),
            messages: vec![Message::user("go")],
            tools: vec![],
            effort: None,
        };
        assert_eq!(cached(&to_chat_request(&req, false).0), [true, true]);

        req.messages.extend([
            Message::assistant(vec![call("c1")]),
            result("c1"),
            Message::assistant(vec![call("c2")]),
            result("c2"),
        ]);
        // system, go, c1 call, c1 result (the previous call's end), c2 call, c2 result
        assert_eq!(
            cached(&to_chat_request(&req, false).0),
            [true, false, false, true, false, true]
        );
    }

    #[test]
    fn without_a_system_prompt_the_last_tool_ends_the_cached_prefix() {
        let spec = |name: &str| ToolSpec {
            name: name.into(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
        };
        let req = Request {
            model: "claude-sonnet-5".into(),
            system: None,
            messages: vec![Message::user("go")],
            tools: vec![spec("Read"), spec("Bash")],
            effort: None,
        };
        let (chat, _) = to_chat_request(&req, false);
        let marked: Vec<_> = chat
            .tools
            .unwrap()
            .iter()
            .map(|t| t.cache_control.is_some())
            .collect();
        assert_eq!(marked, [false, true]);
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
        let (chat, _) = to_chat_request(&req, false);
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
    fn a_completed_reply_with_tool_calls_is_tool_use() {
        // OpenAI's Responses API: `response.completed`'s status, for function calls too
        let mut st = MapState::default();
        let mut deltas = st.map(ChatStreamEvent::ToolCallChunk(ToolChunk {
            tool_call: ToolCall {
                call_id: "c1".into(),
                fn_name: "Bash".into(),
                fn_arguments: Value::String("{\"command\": \"pwd\"}".into()),
                thought_signatures: None,
            },
        }));
        deltas.extend(st.map(end(Some(GStop::Completed("completed".into())), None)));
        let mut acc = Accumulator::default();
        deltas.into_iter().for_each(|d| acc.push(d));
        assert_eq!(acc.finish().stop_reason, StopReason::ToolUse);

        let mut st = MapState::default();
        let mut deltas = st.map(ChatStreamEvent::Chunk(chunk("hi")));
        deltas.extend(st.map(end(Some(GStop::Completed("completed".into())), None)));
        let mut acc = Accumulator::default();
        deltas.into_iter().for_each(|d| acc.push(d));
        assert_eq!(acc.finish().stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn a_stream_that_ended_with_nothing_is_not_a_reply() {
        let st = MapState::default();
        let ChatStreamEvent::End(empty) = end(None, None) else {
            unreachable!()
        };
        assert!(st.ended_empty(&empty));
        let ChatStreamEvent::End(stopped) = end(Some(GStop::Completed("completed".into())), None)
        else {
            unreachable!()
        };
        assert!(!st.ended_empty(&stopped));
        let mut st = MapState::default();
        st.map(ChatStreamEvent::Chunk(chunk("hi")));
        let ChatStreamEvent::End(e) = end(None, None) else {
            unreachable!()
        };
        assert!(!st.ended_empty(&e));
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

    #[test]
    fn http_errors_of_a_streaming_call_are_classified_by_their_status() {
        let streamed = |e: ::genai::Error| ::genai::Error::WebStream {
            model_iden: ::genai::ModelIden::new(::genai::adapter::AdapterKind::Anthropic, "m"),
            cause: "HTTP error.".into(),
            error: Box::new(e),
        };
        let mut h = HeaderMap::new();
        h.insert("retry-after", HeaderValue::from_static("7"));
        assert!(matches!(
            classify_error(streamed(http(429, "slow down", h))),
            ProviderError::RateLimited {
                retry_after: Some(d)
            } if d == Duration::from_secs(7)
        ));
        assert!(matches!(
            classify_error(streamed(http(400, "bad request", HeaderMap::new()))),
            ProviderError::Api {
                status: 400,
                retryable: false,
                ..
            }
        ));
        assert!(matches!(
            classify_error(streamed(http(
                400,
                "prompt is too long: 1000001 tokens",
                HeaderMap::new()
            ))),
            ProviderError::ContextOverflow
        ));
        let cut = ::genai::Error::WebStream {
            model_iden: ::genai::ModelIden::new(::genai::adapter::AdapterKind::Anthropic, "m"),
            cause: "connection reset".into(),
            error: "connection reset".into(),
        };
        assert!(matches!(classify_error(cut), ProviderError::Stream(_)));
    }

    /// The calls made: their bearer tokens and bodies.
    type Sent = Arc<std::sync::Mutex<Vec<(Option<String>, Value)>>>;

    /// OpenAI's Responses API on a ChatGPT plan, for the tests: it takes the access token
    /// `accepts` only, and answers with `REPLY` streamed, or fails as `fail` says. What it's sent
    /// is kept.
    struct Fake {
        url: String,
        requests: Sent,
    }

    const REPLY: &str = "Hello from the plan.";

    async fn fake(accepts: &'static str, fail: Option<(u16, &'static str)>) -> Fake {
        use axum::http::{HeaderMap, StatusCode};
        use axum::response::IntoResponse;
        let requests: Sent = Default::default();
        let kept = requests.clone();
        let app = axum::Router::new().route(
            "/v1/responses",
            axum::routing::post(move |headers: HeaderMap, body: axum::Json<Value>| {
                let kept = kept.clone();
                async move {
                    let bearer = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("Bearer "))
                        .map(String::from);
                    kept.lock().unwrap().push((bearer.clone(), body.0.clone()));
                    if bearer.as_deref() != Some(accepts) {
                        return (StatusCode::UNAUTHORIZED, r#"{"detail":"no"}"#).into_response();
                    }
                    if let Some((status, code)) = fail {
                        let body = json!({ "error": { "code": code, "message": code, "type": "x" } });
                        return (StatusCode::from_u16(status).unwrap(), body.to_string())
                            .into_response();
                    }
                    let item = json!({
                        "id": "msg_1", "type": "message", "role": "assistant", "status": "completed",
                        "content": [{ "type": "output_text", "text": REPLY, "annotations": [] }],
                    });
                    // its reasoning, summarized, and encrypted (the summary's signature)
                    let reasoning = json!({
                        "id": "rs_1", "type": "reasoning", "encrypted_content": "ENC1",
                        "summary": [{ "type": "summary_text", "text": "thinking it over" }],
                    });
                    let events = [
                        json!({ "type": "response.created",
                            "response": { "id": "r", "model": "gpt-test", "status": "in_progress", "output": [] } }),
                        json!({ "type": "response.output_item.added", "output_index": 0,
                            "item": { "id": "rs_1", "type": "reasoning", "summary": [] } }),
                        json!({ "type": "response.reasoning_summary_text.delta", "item_id": "rs_1",
                            "output_index": 0, "summary_index": 0, "delta": "thinking it over" }),
                        json!({ "type": "response.output_item.done", "output_index": 0,
                            "item": reasoning }),
                        json!({ "type": "response.output_item.added", "output_index": 1,
                            "item": { "id": "msg_1", "type": "message", "role": "assistant",
                                "status": "in_progress", "content": [] } }),
                        json!({ "type": "response.output_text.delta", "item_id": "msg_1",
                            "output_index": 1, "content_index": 0, "delta": REPLY }),
                        json!({ "type": "response.output_item.done", "output_index": 1, "item": item }),
                        json!({ "type": "response.completed", "response": {
                            "id": "r", "model": "gpt-test", "status": "completed", "output": [reasoning, item],
                            "usage": { "input_tokens": 10, "output_tokens": 5, "total_tokens": 15,
                                "input_tokens_details": { "cached_tokens": 0 },
                                "output_tokens_details": { "reasoning_tokens": 0 } } } }),
                    ];
                    let sse: String = events
                        .iter()
                        .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
                        .collect();
                    ([("content-type", "text/event-stream")], sse).into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Fake { url, requests }
    }

    /// A lease's plan tokens, `token` to begin with: asked afresh, `renewed` (control's part),
    /// or why there's none.
    fn plan(
        token: Option<&str>,
        renewed: Result<&'static str, &'static str>,
    ) -> (PlanTokens, Arc<std::sync::atomic::AtomicUsize>) {
        let tokens =
            |openai: Option<&str>, why: Option<&str>| dirigent_protocol::control::pb::Tokens {
                player: "p".into(),
                owner: None,
                openai: openai.map(String::from),
                openai_unavailable: why.map(String::from),
                ..Default::default()
            };
        let (tx, rx) =
            tokio::sync::watch::channel(tokens(token, token.is_none().then_some("not linked")));
        let (refresh, mut refreshes) = tokio::sync::mpsc::channel::<bool>(4);
        let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = asked.clone();
        tokio::spawn(async move {
            while let Some(rejected) = refreshes.recv().await {
                assert!(rejected);
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                tx.send_replace(match renewed {
                    Ok(t) => tokens(Some(t), None),
                    Err(why) => tokens(None, Some(why)),
                });
            }
        });
        (PlanTokens::new(rx, refresh), asked)
    }

    fn plan_request() -> Request {
        Request {
            model: "gpt-test".into(),
            system: Some("be brief".into()),
            messages: vec![Message::user("hi")],
            tools: vec![ToolSpec {
                name: "read".into(),
                description: "reads".into(),
                input_schema: json!({ "type": "object", "properties": {} }),
            }],
            effort: Some(Effort::Medium),
        }
    }

    async fn complete(p: &GenaiProvider, req: &Request) -> Result<Completion, ProviderError> {
        let mut stream = p.stream(req).await?;
        let mut acc = Accumulator::default();
        while let Some(delta) = stream.next().await {
            acc.push(delta?);
        }
        Ok(acc.finish())
    }

    #[tokio::test]
    async fn an_anthropic_call_without_a_key_fails_with_controls_word() {
        let why = "no Anthropic API key: add yours in the web's account menu";
        let key = AnthropicKey::of(dirigent_protocol::control::pb::Tokens {
            anthropic_unavailable: Some(why.into()),
            ..Default::default()
        });
        let p = GenaiProvider::anthropic(key, Some("http://127.0.0.1:1/v1/".into())).unwrap();
        match p.stream(&plan_request()).await {
            Err(ProviderError::Api {
                status: 401,
                message,
                retryable: false,
            }) => assert_eq!(message, why),
            Err(other) => panic!("{other:?}"),
            Ok(_) => panic!("a call without a key"),
        }
    }

    #[tokio::test]
    async fn a_chatgpt_plans_call_is_renewed_once_refused_and_says_its_prompt_as_instructions() {
        let api = fake("at-2", None).await;
        let (tokens, asked) = plan(Some("at-1"), Ok("at-2"));
        let p = GenaiProvider::chatgpt(tokens, format!("{}/v1/", api.url)).unwrap();
        let done = complete(&p, &plan_request()).await.unwrap();
        assert_eq!(done.message.text(), REPLY);
        assert_eq!(done.stop_reason, StopReason::EndTurn);
        assert_eq!((done.usage.input, done.usage.output), (10, 5));
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
        let requests = api.requests.lock().unwrap().clone();
        let bearers: Vec<_> = requests.iter().map(|(b, _)| b.as_deref()).collect();
        assert_eq!(bearers, [Some("at-1"), Some("at-2")]);
        let (_, body) = &requests[1];
        assert_eq!(body["model"], "gpt-test");
        assert_eq!(body["instructions"], "be brief");
        assert_eq!(
            (&body["store"], &body["stream"]),
            (&json!(false), &json!(true))
        );
        let input = body["input"].as_array().unwrap();
        assert!(
            input
                .iter()
                .all(|i| i["role"] != "system" && i["role"] != "developer"),
            "{input:?}"
        );
        // what a plan's calls may not have
        for key in [
            "temperature",
            "max_output_tokens",
            "prompt_cache_retention",
            "previous_response_id",
        ] {
            assert!(body.get(key).is_none(), "{key} in {body}");
        }
        assert_eq!(body["tools"][0]["name"], "read");
        // the reasoning, its summary signed with what OpenAI encrypted, goes back with the next
        // call (which takes the renewed token at once)
        assert_eq!(
            done.message.content[0],
            ContentBlock::Thinking {
                text: "thinking it over".into(),
                provider_data: json!({ "signature": "ENC1" }),
            }
        );
        let mut next = plan_request();
        next.messages
            .extend([done.message.clone(), Message::user("and then?")]);
        complete(&p, &next).await.unwrap();
        let (_, body) = api.requests.lock().unwrap().last().unwrap().clone();
        let reasoning: Vec<_> = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["type"] == "reasoning")
            .map(|i| i["encrypted_content"].clone())
            .collect();
        assert_eq!(reasoning, [json!("ENC1")]);
        assert_eq!(
            api.requests.lock().unwrap().last().unwrap().0.as_deref(),
            Some("at-2")
        );
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_chatgpt_plan_out_of_usage_or_unlinked_fails_the_call_for_good() {
        let api = fake(
            "at-1",
            Some((429, "subscription_sharing_usage_limit_exceeded")),
        )
        .await;
        let (tokens, _) = plan(Some("at-1"), Ok("at-1"));
        let p = GenaiProvider::chatgpt(tokens, format!("{}/v1/", api.url)).unwrap();
        let e = complete(&p, &plan_request()).await.unwrap_err();
        assert!(!e.retryable(), "{e:?}");
        assert!(e.to_string().contains("usage limit"), "{e}");

        let api = fake("at-1", None).await;
        let (tokens, _) = plan(None, Ok("at-1"));
        let p = GenaiProvider::chatgpt(tokens, format!("{}/v1/", api.url)).unwrap();
        let e = complete(&p, &plan_request()).await.unwrap_err();
        assert!(
            !e.retryable() && e.to_string().contains("not linked"),
            "{e}"
        );
        assert!(api.requests.lock().unwrap().is_empty());

        // refused, and control can't renew it: why, without calling again
        let (tokens, _) = plan(Some("at-0"), Err("the ChatGPT link is broken: revoked"));
        let p = GenaiProvider::chatgpt(tokens, format!("{}/v1/", api.url)).unwrap();
        let e = complete(&p, &plan_request()).await.unwrap_err();
        assert!(!e.retryable() && e.to_string().contains("revoked"), "{e}");
        assert_eq!(api.requests.lock().unwrap().len(), 1);
    }
}
