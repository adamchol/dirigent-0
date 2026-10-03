pub mod genai;
pub mod mock;

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        provider_data: Value,
    },
    ToolCall {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        call_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    #[cfg_attr(not(test), allow(dead_code))] // the fold builds user messages directly
    pub fn user(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Message {
            role: Role::Assistant,
            content,
        }
    }

    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    High,
    Max,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub effort: Option<Effort>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    Refusal,
    ContextOverflow,
    Other { raw: String },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Delta {
    BlockStart { index: usize, block: ContentBlock },
    Text { index: usize, text: String },
    Thinking { index: usize, text: String },
    ToolCallArgs { index: usize, json: String },
    BlockData { index: usize, provider_data: Value },
    BlockStop { index: usize },
    Usage(Usage),
    Stop(StopReason),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Completion {
    pub message: Message,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("rate limited (retry after {retry_after:?})")]
    RateLimited { retry_after: Option<Duration> },
    #[error("context window exceeded")]
    ContextOverflow,
    #[error("api error {status}: {message}")]
    Api {
        status: u16,
        message: String,
        retryable: bool,
    },
    #[error("stream error: {0}")]
    Stream(String),
    #[error("transport error: {0}")]
    Transport(String),
}

impl ProviderError {
    pub fn retryable(&self) -> bool {
        match self {
            ProviderError::RateLimited { .. } => true,
            ProviderError::ContextOverflow => false,
            ProviderError::Api { retryable, .. } => *retryable,
            ProviderError::Stream(_) | ProviderError::Transport(_) => true,
        }
    }
}

pub type DeltaStream = BoxStream<'static, Result<Delta, ProviderError>>;

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &'static str;

    async fn stream(&self, req: &Request) -> Result<DeltaStream, ProviderError>;
}

#[derive(Debug)]
enum Partial {
    Text(String),
    Thinking {
        text: String,
        provider_data: Value,
    },
    ToolCall {
        id: String,
        name: String,
        json: String,
    },
    ToolResult(ContentBlock),
}

#[derive(Debug, Default)]
pub struct Accumulator {
    blocks: BTreeMap<usize, Partial>,
    usage: Usage,
    stop_reason: Option<StopReason>,
}

impl Accumulator {
    pub fn push(&mut self, delta: Delta) {
        match delta {
            Delta::BlockStart { index, block } => {
                let partial = match block {
                    ContentBlock::Text { text } => Partial::Text(text),
                    ContentBlock::Thinking {
                        text,
                        provider_data,
                    } => Partial::Thinking {
                        text,
                        provider_data,
                    },
                    ContentBlock::ToolCall { id, name, input } => Partial::ToolCall {
                        id,
                        name,
                        json: if input.is_null() {
                            String::new()
                        } else {
                            input.to_string()
                        },
                    },
                    other @ ContentBlock::ToolResult { .. } => Partial::ToolResult(other),
                };
                self.blocks.insert(index, partial);
            }
            Delta::Text { index, text } => match self.blocks.get_mut(&index) {
                Some(Partial::Text(buf)) => buf.push_str(&text),
                _ => {
                    self.blocks.insert(index, Partial::Text(text));
                }
            },
            Delta::Thinking { index, text } => match self.blocks.get_mut(&index) {
                Some(Partial::Thinking { text: buf, .. }) => buf.push_str(&text),
                _ => {
                    self.blocks.insert(
                        index,
                        Partial::Thinking {
                            text,
                            provider_data: Value::Null,
                        },
                    );
                }
            },
            Delta::ToolCallArgs { index, json } => {
                if let Some(Partial::ToolCall { json: buf, .. }) = self.blocks.get_mut(&index) {
                    *buf = json;
                }
            }
            Delta::BlockData {
                index,
                provider_data,
            } => {
                if let Some(Partial::Thinking {
                    provider_data: existing,
                    ..
                }) = self.blocks.get_mut(&index)
                {
                    merge_objects(existing, provider_data);
                }
            }
            Delta::BlockStop { .. } => {}
            Delta::Usage(usage) => self.usage = usage,
            Delta::Stop(reason) => self.stop_reason = Some(reason),
        }
    }

    pub fn finish(self) -> Completion {
        let mut has_tool_call = false;
        let content = self
            .blocks
            .into_values()
            .map(|p| match p {
                Partial::Text(text) => ContentBlock::Text { text },
                Partial::Thinking {
                    text,
                    provider_data,
                } => ContentBlock::Thinking {
                    text,
                    provider_data,
                },
                Partial::ToolCall { id, name, json } => {
                    has_tool_call = true;
                    ContentBlock::ToolCall {
                        id,
                        name,
                        input: parse_tool_input(&json),
                    }
                }
                Partial::ToolResult(block) => block,
            })
            .collect();
        let stop_reason = self.stop_reason.unwrap_or(if has_tool_call {
            StopReason::ToolUse
        } else {
            StopReason::EndTurn
        });
        Completion {
            message: Message::assistant(content),
            stop_reason,
            usage: self.usage,
        }
    }
}

/// Tool input JSON as sent by the model. Empty means `{}`; invalid JSON is
/// surfaced to the tool layer instead of crashing the turn.
pub fn parse_tool_input(json: &str) -> Value {
    if json.trim().is_empty() {
        return Value::Object(Default::default());
    }
    match serde_json::from_str::<Value>(json) {
        Ok(v) => v,
        Err(e) => serde_json::json!({ "__invalid_json": json, "__error": e.to_string() }),
    }
}

/// Merge `incoming` into `target`; string fields are concatenated because
/// signatures can arrive in pieces.
fn merge_objects(target: &mut Value, incoming: Value) {
    let Value::Object(incoming) = incoming else {
        return;
    };
    if !target.is_object() {
        *target = Value::Object(Default::default());
    }
    let map = target.as_object_mut().expect("object");
    for (k, v) in incoming {
        match (map.get_mut(&k), v) {
            (Some(Value::String(existing)), Value::String(more)) => existing.push_str(&more),
            (_, v) => {
                map.insert(k, v);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accumulates_text_and_tool_call() {
        let mut acc = Accumulator::default();
        acc.push(Delta::BlockStart {
            index: 0,
            block: ContentBlock::Text {
                text: String::new(),
            },
        });
        acc.push(Delta::Text {
            index: 0,
            text: "Hel".into(),
        });
        acc.push(Delta::Text {
            index: 0,
            text: "lo".into(),
        });
        acc.push(Delta::BlockStop { index: 0 });
        acc.push(Delta::BlockStart {
            index: 1,
            block: ContentBlock::ToolCall {
                id: "t1".into(),
                name: "Read".into(),
                input: Value::Null,
            },
        });
        acc.push(Delta::ToolCallArgs {
            index: 1,
            json: "{\"file".into(),
        });
        acc.push(Delta::ToolCallArgs {
            index: 1,
            json: "{\"file_path\":\"/a\"}".into(),
        });
        acc.push(Delta::Usage(Usage {
            input: 10,
            output: 5,
            cache_read: 0,
            cache_write: 0,
        }));
        acc.push(Delta::Stop(StopReason::ToolUse));
        let c = acc.finish();
        assert_eq!(
            c.message.content,
            vec![
                ContentBlock::Text {
                    text: "Hello".into()
                },
                ContentBlock::ToolCall {
                    id: "t1".into(),
                    name: "Read".into(),
                    input: json!({"file_path": "/a"}),
                }
            ]
        );
        assert_eq!(c.stop_reason, StopReason::ToolUse);
        assert_eq!(c.usage.input, 10);
    }

    #[test]
    fn thinking_signature_is_kept_and_invalid_json_is_surfaced() {
        let mut acc = Accumulator::default();
        acc.push(Delta::Thinking {
            index: 0,
            text: "hmm".into(),
        });
        acc.push(Delta::BlockData {
            index: 0,
            provider_data: json!({"signature": "abc"}),
        });
        acc.push(Delta::BlockData {
            index: 0,
            provider_data: json!({"signature": "def"}),
        });
        acc.push(Delta::BlockStart {
            index: 1,
            block: ContentBlock::ToolCall {
                id: "t".into(),
                name: "Bash".into(),
                input: Value::Null,
            },
        });
        acc.push(Delta::ToolCallArgs {
            index: 1,
            json: "{not json".into(),
        });
        let c = acc.finish();
        assert_eq!(
            c.message.content[0],
            ContentBlock::Thinking {
                text: "hmm".into(),
                provider_data: json!({"signature": "abcdef"}),
            }
        );
        match &c.message.content[1] {
            ContentBlock::ToolCall { input, .. } => {
                assert_eq!(input["__invalid_json"], "{not json");
                assert!(input["__error"].is_string());
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(c.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn empty_tool_input_is_empty_object() {
        assert_eq!(parse_tool_input(""), json!({}));
        assert_eq!(parse_tool_input("  "), json!({}));
    }

    #[test]
    fn types_round_trip_through_serde() {
        let msg = Message::assistant(vec![
            ContentBlock::Thinking {
                text: "t".into(),
                provider_data: json!({"signature": "s"}),
            },
            ContentBlock::Text { text: "x".into() },
            ContentBlock::ToolCall {
                id: "1".into(),
                name: "n".into(),
                input: json!({"a": 1}),
            },
        ]);
        let s = serde_json::to_string(&msg).unwrap();
        assert_eq!(serde_json::from_str::<Message>(&s).unwrap(), msg);

        let stop = StopReason::Other {
            raw: "weird".into(),
        };
        let s = serde_json::to_string(&stop).unwrap();
        assert_eq!(s, r#"{"reason":"other","raw":"weird"}"#);
        assert_eq!(serde_json::from_str::<StopReason>(&s).unwrap(), stop);
    }
}
