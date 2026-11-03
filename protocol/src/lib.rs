//! What every component of dirigent shares: a session's events, how they're kept in the event
//! system (one Kafka topic ingested into a ClickHouse table, read back through chlive), how a
//! log of them reads, and how a player holds a session (`control`).

pub mod control;
pub mod fold;
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

/// One event of a session's log. Users write messages and interrupts; the player writes the
/// rest, recording which messages it took and how each turn ended. Ids are other rows' `id`s
/// (see [`rows::EventRow`]); a turn's id is its `TurnStarted`'s.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Event {
    /// From the player, before it reads the log: it holds the session's lease, as the `epoch`th
    /// player to (see `control`).
    PlayerAttached { epoch: i64 },
    /// From the player, last: the session was stopped and it gives the lease back, as the
    /// `epoch`th player.
    PlayerDetached { epoch: i64 },
    /// From the player, last: it failed, `error` says why, and gives the lease back (the session
    /// is idle until it's started again).
    PlayerFailed { epoch: i64, error: String },
    /// From the player, after reading the log: the workspace is in place, from `source` (as in
    /// the spec) at the commit `head`; `recreated` when the log had it ready before but the
    /// player found it gone and made it again, without what the session had changed.
    WorkspaceReady {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<control::Source>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        head: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        recreated: bool,
    },
    /// From the player, after reading the log: it found the workspace gone, and restored it from
    /// the log's last [`Event::Checkpoint`], `snapshot`.
    WorkspaceRestored { snapshot: String },
    /// From the player, after the turn `turn` ended: the workspace as it left it is the snapshot
    /// `snapshot` (kopia's id), which a player that finds the workspace gone restores.
    Checkpoint { turn: String, snapshot: String },
    /// From the player: the turn `turn` got no checkpoint; the session goes on.
    CheckpointFailed { turn: String, error: String },
    /// From a user.
    UserMessage { text: String },
    /// From a user: stop the turn `turn`, the one they saw running.
    UserInterrupt { turn: String },
    /// From the player: a turn on the message `input`.
    TurnStarted { input: String },
    /// From the player: the message `input` joins the running turn.
    Steered { input: String },
    /// From the model task: one streaming delta of the call in flight, for live display only.
    ModelDelta { delta: Delta },
    /// From the model task: the finished reply.
    AssistantMessage {
        content: Vec<ContentBlock>,
        stop_reason: StopReason,
        usage: Usage,
    },
    /// From the model task: the call was cancelled before it finished.
    ModelCancelled,
    /// From the model task: the call failed. With `retry_in_ms`, the task calls again after that
    /// long and the turn goes on; with `compact`, the context was too long and the player
    /// compacts it before calling again; with neither, the turn fails.
    Error {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        retry_in_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        compact: bool,
    },
    /// From the player: `summary` replaces the conversation before the handled event
    /// `first_kept`, which is kept as it was along with everything after it. `usage` is the
    /// summary call's.
    Compacted {
        summary: String,
        first_kept: String,
        usage: Usage,
    },
    /// From the player: no summary came of the context; the turn goes on without one.
    CompactionFailed { message: String },
    /// From the tools task.
    ToolResult {
        call_id: String,
        name: String,
        content: String,
        is_error: bool,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        meta: Value,
    },
    /// From the player; `interrupt` is the user interrupt that ended it, if one did.
    TurnEnded {
        reason: TurnEndReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interrupt: Option<String>,
    },
}
