use serde_json::Value;

use crate::provider::{ContentBlock, Delta, StopReason, Usage};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnEndReason {
    Completed,
    Aborted,
    Failed,
}

/// Everything the loop handles. The loop publishes every event it handles,
/// in the order it handles them, plus the `TurnEnded` it derives; that
/// published stream is the session log.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// From the session.
    UserMessage { text: String },
    /// From the session.
    UserInterrupt,
    /// From the model task: one streaming delta. Published for live
    /// display only; the loop takes no decision on it and history skips it.
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
        meta: Value,
    },
    /// From the loop itself.
    TurnEnded { reason: TurnEndReason },
}
