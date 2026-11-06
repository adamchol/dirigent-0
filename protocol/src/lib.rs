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

/// What a [`Event::SessionUpdate`] tells of the session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "what", rename_all = "snake_case")]
pub enum Update {
    TurnEnded { reason: TurnEndReason },
    PlayerFailed { error: String },
    PlayerDetached,
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
    /// From a user; `via`, the dirigent session that wrote it as them, if one did.
    UserMessage {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<String>,
    },
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
    /// From a dirigent's player, into its own log: what happened in the session `session` it
    /// directs, at its log's `offset`; `reply` is the session's last assistant text then, if any.
    /// An input, as a user's message is.
    SessionUpdate {
        session: String,
        offset: u64,
        update: Update,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply: Option<String>,
    },
    /// From the player; `interrupt` is the user interrupt that ended it, if one did.
    TurnEnded {
        reason: TurnEndReason,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        interrupt: Option<String>,
    },
}

impl Event {
    /// Every event's `kind`, as the log stores it (and Postgres' `insert_kinds` lists them).
    pub const KINDS: &[&str] = &[
        "player_attached",
        "player_detached",
        "player_failed",
        "workspace_ready",
        "workspace_restored",
        "checkpoint",
        "checkpoint_failed",
        "user_message",
        "user_interrupt",
        "turn_started",
        "steered",
        "model_delta",
        "assistant_message",
        "model_cancelled",
        "error",
        "compacted",
        "compaction_failed",
        "session_update",
        "tool_result",
        "turn_ended",
    ];

    /// An input's text, as the model reads it: a user's message, or a session update; `None`
    /// for anything else.
    pub fn input(&self) -> Option<String> {
        match self {
            Event::UserMessage { text, .. } => Some(text.clone()),
            Event::SessionUpdate {
                session,
                offset,
                update,
                reply,
            } => {
                let what = match update {
                    Update::TurnEnded { reason } => match reason {
                        TurnEndReason::Completed => "its turn completed".to_string(),
                        TurnEndReason::Aborted => "its turn was interrupted".to_string(),
                        TurnEndReason::Failed => "its turn failed".to_string(),
                    },
                    Update::PlayerFailed { error } => format!("its player failed: {error}"),
                    Update::PlayerDetached => "it was stopped".to_string(),
                };
                let reply = match reply {
                    Some(r) => format!("\n\nIts last reply:\n{r}"),
                    None => String::new(),
                };
                Some(format!(
                    "<session_update session=\"{session}\" offset=\"{offset}\">{what}.{reply}</session_update>"
                ))
            }
            _ => None,
        }
    }

    /// Its `kind`, as the log stores it.
    pub fn kind(&self) -> String {
        match serde_json::to_value(self) {
            Ok(Value::Object(v)) => match v.get("kind") {
                Some(Value::String(k)) => k.clone(),
                _ => unreachable!("events are tagged"),
            },
            _ => unreachable!("events serialize to objects"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_are_every_events_and_postgres_grants_them() {
        // one of each, by a match that breaks when a variant comes
        let one = |e: &Event| match e {
            Event::PlayerAttached { .. }
            | Event::PlayerDetached { .. }
            | Event::PlayerFailed { .. }
            | Event::WorkspaceReady { .. }
            | Event::WorkspaceRestored { .. }
            | Event::Checkpoint { .. }
            | Event::CheckpointFailed { .. }
            | Event::UserMessage { .. }
            | Event::UserInterrupt { .. }
            | Event::TurnStarted { .. }
            | Event::Steered { .. }
            | Event::ModelDelta { .. }
            | Event::AssistantMessage { .. }
            | Event::ModelCancelled
            | Event::Error { .. }
            | Event::Compacted { .. }
            | Event::CompactionFailed { .. }
            | Event::SessionUpdate { .. }
            | Event::ToolResult { .. }
            | Event::TurnEnded { .. } => e.kind(),
        };
        let s = String::new;
        let usage = Usage::default();
        let events = [
            Event::PlayerAttached { epoch: 1 },
            Event::PlayerDetached { epoch: 1 },
            Event::PlayerFailed {
                epoch: 1,
                error: s(),
            },
            Event::WorkspaceReady {
                source: None,
                head: None,
                recreated: false,
            },
            Event::WorkspaceRestored { snapshot: s() },
            Event::Checkpoint {
                turn: s(),
                snapshot: s(),
            },
            Event::CheckpointFailed {
                turn: s(),
                error: s(),
            },
            Event::UserMessage {
                text: s(),
                via: None,
            },
            Event::UserInterrupt { turn: s() },
            Event::TurnStarted { input: s() },
            Event::Steered { input: s() },
            Event::ModelDelta {
                delta: Delta::Usage(usage),
            },
            Event::AssistantMessage {
                content: vec![],
                stop_reason: StopReason::EndTurn,
                usage,
            },
            Event::ModelCancelled,
            Event::Error {
                message: s(),
                retry_in_ms: None,
                compact: false,
            },
            Event::Compacted {
                summary: s(),
                first_kept: s(),
                usage,
            },
            Event::CompactionFailed { message: s() },
            Event::SessionUpdate {
                session: s(),
                offset: 0,
                update: Update::PlayerDetached,
                reply: None,
            },
            Event::ToolResult {
                call_id: s(),
                name: s(),
                content: s(),
                is_error: false,
                meta: Value::Null,
            },
            Event::TurnEnded {
                reason: TurnEndReason::Completed,
                interrupt: None,
            },
        ];
        let kinds: Vec<String> = events.iter().map(one).collect();
        assert_eq!(kinds, Event::KINDS);
        let schema = include_str!("../../db/postgres/schema/dirigent.hcl");
        for k in Event::KINDS {
            assert!(
                schema.contains(&format!("'{k}'")),
                "db/postgres/schema/dirigent.hcl's grant_insert_kinds misses {k}"
            );
        }
    }

    #[test]
    fn a_session_update_reads_as_an_input() {
        let ev = Event::SessionUpdate {
            session: "s1".into(),
            offset: 7,
            update: Update::TurnEnded {
                reason: TurnEndReason::Completed,
            },
            reply: Some("done".into()),
        };
        assert_eq!(
            ev.input().unwrap(),
            "<session_update session=\"s1\" offset=\"7\">its turn completed.\n\nIts last reply:\ndone</session_update>"
        );
        let wire = serde_json::to_value(&ev).unwrap();
        assert_eq!(
            wire["data"]["update"],
            serde_json::json!({"what": "turn_ended", "reason": "completed"})
        );
        assert_eq!(serde_json::from_value::<Event>(wire).unwrap(), ev);
    }
}
