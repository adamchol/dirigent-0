//! How a session lives in the event system: one Kafka topic keyed by the session id (so a
//! session stays in one partition), each message one row of the ClickHouse table of the same
//! name (see `scripts/schema.sql`), read back with chlive queries in offset order.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::Event;

pub const EVENTS: &str = "session_events";

/// The `author` of rows users write.
pub const USER: &str = "user";
/// The `author` of rows the player writes.
pub const PLAYER: &str = "player";

/// A session's log without its streaming deltas, in order (chlive dialect; `session` is the
/// session id).
pub const LOG_QUERY: &str = "select session_id, offset, partition, ts, id, author, kind, data \
     from session_events where session_id = {session:UUID} and kind != 'model_delta' \
     order by offset";
/// A session's log with its streaming deltas, in order.
pub const SESSION_QUERY: &str = "select session_id, offset, partition, ts, id, author, kind, \
     data from session_events where session_id = {session:UUID} order by offset";

#[derive(Debug, thiserror::Error)]
#[error("bad {what}: {source}")]
pub struct RowError {
    what: &'static str,
    source: serde_json::Error,
}

/// One row of the log: `kind` is the event's type, `data` its fields as JSON (empty for none).
/// `offset`, `partition` and `ts` are the Kafka message's, added on ingestion (and by chlive on
/// live rows), so a writer never sends them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRow {
    pub session_id: String,
    #[serde(default, skip_serializing, deserialize_with = "u64_lenient")]
    pub offset: u64,
    #[serde(default, skip_serializing, deserialize_with = "u64_lenient")]
    pub partition: u64,
    #[serde(default, skip_serializing, deserialize_with = "string_lenient")]
    pub ts: String,
    pub id: String,
    pub author: String,
    pub kind: String,
    pub data: String,
}

impl EventRow {
    /// A row to send; `id` must be new (a UUID).
    pub fn new(session_id: &str, id: String, author: &str, event: &Event) -> Self {
        let Value::Object(mut v) = serde_json::to_value(event).expect("events serialize") else {
            unreachable!("events serialize to objects")
        };
        let kind = match v.remove("kind") {
            Some(Value::String(k)) => k,
            _ => unreachable!("events are tagged"),
        };
        let data = v.remove("data").map(|d| d.to_string()).unwrap_or_default();
        EventRow {
            session_id: session_id.to_string(),
            offset: 0,
            partition: 0,
            ts: String::new(),
            id,
            author: author.to_string(),
            kind,
            data,
        }
    }

    pub fn event(&self) -> Result<Event, RowError> {
        let mut v = Map::new();
        v.insert("kind".into(), Value::String(self.kind.clone()));
        if !self.data.is_empty() {
            let data = serde_json::from_str(&self.data).map_err(|source| RowError {
                what: "event data",
                source,
            })?;
            v.insert("data".into(), data);
        }
        serde_json::from_value(Value::Object(v)).map_err(|source| RowError {
            what: "event",
            source,
        })
    }
}

/// A `UInt64` as ClickHouse renders it: a number, or quoted.
fn u64_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    match Value::deserialize(d)? {
        Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| serde::de::Error::custom(format!("not a UInt64: {n}"))),
        Value::String(s) => s.parse().map_err(serde::de::Error::custom),
        other => Err(serde::de::Error::custom(format!("not a UInt64: {other}"))),
    }
}

/// A value shown as text, whatever JSON type it came as.
fn string_lenient<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) => s,
        Value::Null => String::new(),
        other => other.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentBlock, Delta, StopReason, TurnEndReason, Usage};
    use serde_json::json;

    fn every_event() -> Vec<Event> {
        vec![
            Event::UserMessage { text: "hi".into() },
            Event::UserInterrupt { turn: "t1".into() },
            Event::TurnStarted { input: "m1".into() },
            Event::Steered { input: "m2".into() },
            Event::ModelDelta {
                delta: Delta::Text {
                    index: 0,
                    text: "he".into(),
                },
            },
            Event::AssistantMessage {
                content: vec![
                    ContentBlock::Thinking {
                        text: "t".into(),
                        provider_data: json!({"signature": "s"}),
                    },
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
            Event::ModelCancelled,
            Event::Error {
                message: "boom".into(),
                retry_in_ms: Some(1000),
                compact: false,
            },
            Event::Error {
                message: "too long".into(),
                retry_in_ms: None,
                compact: true,
            },
            Event::Compacted {
                summary: "s".into(),
                first_kept: "e7".into(),
                usage: Usage {
                    input: 1,
                    output: 2,
                    cache_read: 3,
                    cache_write: 4,
                },
            },
            Event::CompactionFailed {
                message: "empty".into(),
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
                interrupt: Some("i1".into()),
            },
            Event::TurnEnded {
                reason: TurnEndReason::Completed,
                interrupt: None,
            },
        ]
    }

    #[test]
    fn every_event_round_trips_through_a_row() {
        for (n, ev) in every_event().into_iter().enumerate() {
            let row = EventRow::new("s", n.to_string(), PLAYER, &ev);
            let wire = serde_json::to_string(&row).unwrap();
            let back: EventRow = serde_json::from_str(&wire).unwrap();
            assert_eq!(back.event().unwrap(), ev, "{wire}");
        }
    }

    #[test]
    fn rows_split_the_type_from_the_fields() {
        let ended = Event::TurnEnded {
            reason: TurnEndReason::Completed,
            interrupt: None,
        };
        let row = EventRow::new("s", "e1".into(), PLAYER, &ended);
        assert_eq!(row.kind, "turn_ended");
        assert_eq!(row.data, r#"{"reason":"completed"}"#);
        let row = EventRow::new("s", "e2".into(), PLAYER, &Event::ModelCancelled);
        assert_eq!(
            (row.kind.as_str(), row.data.as_str()),
            ("model_cancelled", "")
        );
        let failed = Event::Error {
            message: "x".into(),
            retry_in_ms: None,
            compact: false,
        };
        let row = EventRow::new("s", "e3".into(), PLAYER, &failed);
        assert_eq!(row.data, r#"{"message":"x"}"#);
    }

    #[test]
    fn the_kafka_fields_are_read_but_never_sent() {
        let row = EventRow::new(
            "s",
            "e1".into(),
            USER,
            &Event::UserMessage { text: "a".into() },
        );
        let sent = serde_json::to_value(&row).unwrap();
        for field in ["offset", "partition", "ts"] {
            assert!(sent.get(field).is_none(), "{field} sent: {sent}");
        }
        let read: EventRow = serde_json::from_value(json!({
            "session_id": "s", "offset": "18446744073709551615", "partition": 2,
            "ts": "2026-10-31 12:00:00.123", "id": "e1", "author": "user",
            "kind": "model_cancelled", "data": ""
        }))
        .unwrap();
        assert_eq!(
            (read.offset, read.partition, read.ts.as_str()),
            (u64::MAX, 2, "2026-10-31 12:00:00.123")
        );
    }
}
