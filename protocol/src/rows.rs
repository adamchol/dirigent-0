//! How a session lives in the event system: one Kafka topic per table, keyed by the session id
//! (so a session stays in one partition), each message one row of the ClickHouse table of the
//! same name (see `scripts/schema.sql`), read back with chlive queries.

use std::time::SystemTime;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use crate::{Delta, Event};

pub const EVENTS: &str = "session_events";
pub const INPUTS: &str = "session_inputs";
pub const DELTAS: &str = "session_deltas";

/// A session's log, in order (chlive dialect; `session` is the session id).
pub const EVENTS_QUERY: &str = "select session_id, seq, ts, kind, data from session_events \
     where session_id = {session:UUID} order by seq";
/// What users sent to a session.
pub const INPUTS_QUERY: &str = "select session_id, input_id, ts, kind, text from session_inputs \
     where session_id = {session:UUID} order by ts";
/// A session's streaming deltas.
pub const DELTAS_QUERY: &str = "select session_id, after_seq, ts, data from session_deltas \
     where session_id = {session:UUID}";

/// `2026-10-29 12:00:00.123`, as a `DateTime64(3)` column takes it (UTC).
pub fn timestamp(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(t)
        .format("%Y-%m-%d %H:%M:%S%.3f")
        .to_string()
}

#[derive(Debug, thiserror::Error)]
pub enum RowError {
    #[error("bad {what}: {source}")]
    Json {
        what: &'static str,
        source: serde_json::Error,
    },
    #[error("unknown input kind {0:?}")]
    InputKind(String),
}

/// One logged event: `kind` is its type, `data` its fields as JSON (empty for none).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRow {
    pub session_id: String,
    #[serde(deserialize_with = "u64_lenient")]
    pub seq: u64,
    pub ts: String,
    pub kind: String,
    pub data: String,
}

impl EventRow {
    pub fn new(session_id: &str, seq: u64, ts: String, event: &Event) -> Self {
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
            seq,
            ts,
            kind,
            data,
        }
    }

    pub fn event(&self) -> Result<Event, RowError> {
        let mut v = Map::new();
        v.insert("kind".into(), Value::String(self.kind.clone()));
        if !self.data.is_empty() {
            let data = serde_json::from_str(&self.data).map_err(|source| RowError::Json {
                what: "event data",
                source,
            })?;
            v.insert("data".into(), data);
        }
        serde_json::from_value(Value::Object(v)).map_err(|source| RowError::Json {
            what: "event",
            source,
        })
    }
}

/// One streaming delta. `after_seq` is the seq of the last event logged before it, so a reader
/// can tell which model call it belongs to: the one after the last call-ending event at or
/// before `after_seq`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeltaRow {
    pub session_id: String,
    #[serde(deserialize_with = "u64_lenient")]
    pub after_seq: u64,
    pub ts: String,
    pub data: String,
}

impl DeltaRow {
    pub fn new(session_id: &str, after_seq: u64, ts: String, delta: &Delta) -> Self {
        DeltaRow {
            session_id: session_id.to_string(),
            after_seq,
            ts,
            data: serde_json::to_string(delta).expect("deltas serialize"),
        }
    }

    pub fn delta(&self) -> Result<Delta, RowError> {
        serde_json::from_str(&self.data).map_err(|source| RowError::Json {
            what: "delta",
            source,
        })
    }
}

/// What a user sends to a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputRow {
    pub session_id: String,
    pub input_id: String,
    pub ts: String,
    /// `message` or `interrupt`.
    pub kind: String,
    #[serde(default)]
    pub text: String,
}

impl InputRow {
    pub fn message(session_id: &str, input_id: String, ts: String, text: String) -> Self {
        InputRow {
            session_id: session_id.to_string(),
            input_id,
            ts,
            kind: "message".into(),
            text,
        }
    }

    pub fn interrupt(session_id: &str, input_id: String, ts: String) -> Self {
        InputRow {
            session_id: session_id.to_string(),
            input_id,
            ts,
            kind: "interrupt".into(),
            text: String::new(),
        }
    }

    /// The event a player handles for it.
    pub fn event(&self) -> Result<Event, RowError> {
        let input_id = self.input_id.clone();
        match self.kind.as_str() {
            "message" => Ok(Event::UserMessage {
                input_id,
                text: self.text.clone(),
            }),
            "interrupt" => Ok(Event::UserInterrupt { input_id }),
            other => Err(RowError::InputKind(other.to_string())),
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentBlock, StopReason, TurnEndReason, Usage};
    use serde_json::json;
    use std::time::{Duration, UNIX_EPOCH};

    fn every_event() -> Vec<Event> {
        vec![
            Event::UserMessage {
                input_id: "i1".into(),
                text: "hi".into(),
            },
            Event::UserInterrupt {
                input_id: "i2".into(),
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
        ]
    }

    #[test]
    fn every_event_round_trips_through_a_row() {
        for (seq, ev) in every_event().into_iter().enumerate() {
            let row = EventRow::new("s", seq as u64, "t".into(), &ev);
            let wire = serde_json::to_string(&row).unwrap();
            let back: EventRow = serde_json::from_str(&wire).unwrap();
            assert_eq!(back.event().unwrap(), ev, "{wire}");
        }
    }

    #[test]
    fn rows_split_the_type_from_the_fields() {
        let row = EventRow::new(
            "s",
            7,
            "t".into(),
            &Event::TurnEnded {
                reason: TurnEndReason::Completed,
            },
        );
        assert_eq!(row.kind, "turn_ended");
        assert_eq!(row.data, r#"{"reason":"completed"}"#);
        let row = EventRow::new("s", 8, "t".into(), &Event::ModelCancelled);
        assert_eq!(
            (row.kind.as_str(), row.data.as_str()),
            ("model_cancelled", "")
        );
    }

    #[test]
    fn seq_reads_numbers_and_quoted_numbers() {
        let row: EventRow = serde_json::from_value(json!({
            "session_id": "s", "seq": "18446744073709551615", "ts": "t",
            "kind": "model_cancelled", "data": ""
        }))
        .unwrap();
        assert_eq!(row.seq, u64::MAX);
        let row: DeltaRow = serde_json::from_value(json!({
            "session_id": "s", "after_seq": 3, "ts": "t", "data": "{}"
        }))
        .unwrap();
        assert_eq!(row.after_seq, 3);
    }

    #[test]
    fn deltas_and_inputs() {
        let d = Delta::Text {
            index: 0,
            text: "he".into(),
        };
        assert_eq!(DeltaRow::new("s", 1, "t".into(), &d).delta().unwrap(), d);

        let m = InputRow::message("s", "i".into(), "t".into(), "go".into());
        assert_eq!(
            m.event().unwrap(),
            Event::UserMessage {
                input_id: "i".into(),
                text: "go".into()
            }
        );
        let i = InputRow::interrupt("s", "j".into(), "t".into());
        assert_eq!(
            i.event().unwrap(),
            Event::UserInterrupt {
                input_id: "j".into()
            }
        );
        let mut bad = i;
        bad.kind = "nope".into();
        assert!(bad.event().is_err());
    }

    #[test]
    fn timestamps_are_utc_millis() {
        let t = UNIX_EPOCH + Duration::from_millis(1_793_000_000_123);
        assert_eq!(timestamp(t), "2026-10-26 07:33:20.123");
    }
}
