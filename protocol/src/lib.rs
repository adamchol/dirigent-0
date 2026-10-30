//! What every component of dirigent shares: a session's events, and how they're kept in the
//! event system (Kafka topics ingested into ClickHouse tables, read back through chlive).

pub mod rows;

use serde::{Deserialize, Serialize};
use serde_json::Value;

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

/// One streaming piece of a model reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Delta {
    BlockStart {
        index: usize,
        block: ContentBlock,
    },
    Text {
        index: usize,
        text: String,
    },
    Thinking {
        index: usize,
        text: String,
    },
    /// The tool call's arguments so far (replaces earlier ones for the index).
    ToolCallArgs {
        index: usize,
        json: String,
    },
    BlockData {
        index: usize,
        provider_data: Value,
    },
    BlockStop {
        index: usize,
    },
    Usage(Usage),
    Stop(StopReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndReason {
    Completed,
    Aborted,
    Failed,
}

/// Everything a player handles. It publishes every event it handles, in the order it handles
/// them, plus the `TurnEnded` it derives; that published stream is the session log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Event {
    /// From a user; `input_id` is the input it came as.
    UserMessage { input_id: String, text: String },
    /// From a user.
    UserInterrupt { input_id: String },
    /// From the model task: one streaming delta, for live display only; never kept in history.
    ModelDelta { delta: Delta },
    /// From the model task: the finished reply.
    AssistantMessage {
        content: Vec<ContentBlock>,
        stop_reason: StopReason,
        usage: Usage,
    },
    /// From the model task: the call was cancelled before it finished.
    ModelCancelled,
    /// From the model task: the call failed.
    Error { message: String, retryable: bool },
    /// From the tools task.
    ToolResult {
        call_id: String,
        name: String,
        content: String,
        is_error: bool,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        meta: Value,
    },
    /// From the player itself.
    TurnEnded { reason: TurnEndReason },
}
