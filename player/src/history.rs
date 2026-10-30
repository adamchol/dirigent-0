//! The only state the loop keeps across turns: what the next model call
//! needs. Built from events, live (`push`) or on resume (`from_events`).

use crate::provider::{ContentBlock, Message, Role};
use dirigent_protocol::{Event, TurnEndReason};

pub const ABORT_MARKER: &str =
    "The user interrupted the previous turn. Tools that were running may have partially executed.";

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    User {
        text: String,
    },
    Assistant {
        content: Vec<ContentBlock>,
    },
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
    /// Turn boundary: decides whether a later user message starts a turn or
    /// steers one, and carries the interrupt marker.
    TurnEnded {
        aborted: bool,
    },
}

impl Entry {
    /// The single conversion from events, used live and on resume.
    pub fn from_event(ev: &Event) -> Option<Entry> {
        match ev {
            Event::UserMessage { text, .. } => Some(Entry::User { text: text.clone() }),
            Event::AssistantMessage { content, .. } => Some(Entry::Assistant {
                content: content.clone(),
            }),
            Event::ToolResult {
                call_id,
                content,
                is_error,
                ..
            } => Some(Entry::ToolResult {
                call_id: call_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            }),
            Event::TurnEnded { reason } => Some(Entry::TurnEnded {
                aborted: *reason == TurnEndReason::Aborted,
            }),
            Event::UserInterrupt { .. }
            | Event::ModelDelta { .. }
            | Event::ModelCancelled
            | Event::Error { .. } => None,
        }
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct History(Vec<Entry>);

impl History {
    pub fn from_events(events: &[Event]) -> Self {
        History(events.iter().filter_map(Entry::from_event).collect())
    }

    pub fn push(&mut self, ev: &Event) {
        if let Some(entry) = Entry::from_event(ev) {
            self.0.push(entry);
        }
    }

    /// The model's conversation. A user message sent while a model call was
    /// in flight lands after that call's reply; tool results open the next
    /// user message, in call order.
    pub fn messages(&self) -> Vec<Message> {
        let mut out: Vec<Message> = Vec::new();
        let mut results: Vec<ContentBlock> = Vec::new();
        let mut texts: Vec<String> = Vec::new();
        let mut held: Vec<String> = Vec::new();
        let mut calls: Vec<String> = Vec::new();
        let mut in_turn = false;
        let mut calling = false;

        for entry in &self.0 {
            match entry {
                Entry::User { text } if !in_turn => {
                    in_turn = true;
                    calling = true;
                    texts.push(text.clone());
                }
                Entry::User { text } if calling => held.push(text.clone()),
                Entry::User { text } => texts.push(text.clone()),
                Entry::Assistant { content } => {
                    flush(&mut out, &mut results, &mut texts, &calls);
                    calls = content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolCall { id, .. } => Some(id.clone()),
                            _ => None,
                        })
                        .collect();
                    out.push(Message::assistant(content.clone()));
                    texts.append(&mut held);
                    // A steer during a text-only reply triggers another call.
                    calling = calls.is_empty() && !texts.is_empty();
                }
                Entry::ToolResult {
                    call_id,
                    content,
                    is_error,
                } => {
                    results.push(ContentBlock::ToolResult {
                        call_id: call_id.clone(),
                        content: content.clone(),
                        is_error: *is_error,
                    });
                    calling = results.len() >= calls.len();
                }
                Entry::TurnEnded { aborted } => {
                    in_turn = false;
                    calling = false;
                    texts.append(&mut held);
                    if *aborted {
                        texts.push(ABORT_MARKER.into());
                    }
                }
            }
        }
        flush(&mut out, &mut results, &mut texts, &calls);
        merge_adjacent(out)
    }
}

/// Emit pending tool results (in call order) and texts as one user message.
fn flush(
    out: &mut Vec<Message>,
    results: &mut Vec<ContentBlock>,
    texts: &mut Vec<String>,
    calls: &[String],
) {
    let mut content: Vec<ContentBlock> = Vec::new();
    for id in calls {
        if let Some(pos) = results
            .iter()
            .position(|b| matches!(b, ContentBlock::ToolResult { call_id, .. } if call_id == id))
        {
            content.push(results.remove(pos));
        }
    }
    content.append(results);
    content.extend(texts.drain(..).map(|text| ContentBlock::Text { text }));
    if !content.is_empty() {
        out.push(Message {
            role: Role::User,
            content,
        });
    }
}

fn merge_adjacent(messages: Vec<Message>) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    for m in messages {
        match out.last_mut() {
            Some(last) if last.role == m.role => {
                last.content.extend(m.content);
                if m.role == Role::User {
                    let (results, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut last.content)
                        .into_iter()
                        .partition(|b| matches!(b, ContentBlock::ToolResult { .. }));
                    last.content = results;
                    last.content.extend(rest);
                }
            }
            _ => out.push(m),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{StopReason, Usage};
    use serde_json::{json, Value};

    fn user(text: &str) -> Event {
        Event::UserMessage {
            input_id: format!("in-{text}"),
            text: text.into(),
        }
    }

    fn interrupt() -> Event {
        Event::UserInterrupt {
            input_id: "in-interrupt".into(),
        }
    }

    fn assistant(content: Vec<ContentBlock>, stop: StopReason) -> Event {
        Event::AssistantMessage {
            content,
            stop_reason: stop,
            usage: Usage::default(),
        }
    }

    fn call(id: &str) -> ContentBlock {
        ContentBlock::ToolCall {
            id: id.into(),
            name: "Read".into(),
            input: json!({}),
        }
    }

    fn result(id: &str) -> Event {
        Event::ToolResult {
            call_id: id.into(),
            name: "Read".into(),
            content: format!("out {id}"),
            is_error: false,
            meta: Value::Null,
        }
    }

    fn ended(reason: TurnEndReason) -> Event {
        Event::TurnEnded { reason }
    }

    fn text(t: &str) -> ContentBlock {
        ContentBlock::Text { text: t.into() }
    }

    fn messages(events: &[Event]) -> Vec<Message> {
        History::from_events(events).messages()
    }

    #[test]
    fn keeps_only_model_relevant_events() {
        let events = vec![
            user("a"),
            interrupt(),
            Event::ModelDelta {
                delta: crate::provider::Delta::BlockStop { index: 0 },
            },
            Event::ModelCancelled,
            Event::Error {
                message: "x".into(),
                retryable: true,
            },
            ended(TurnEndReason::Failed),
        ];
        assert_eq!(
            History::from_events(&events),
            History(vec![
                Entry::User { text: "a".into() },
                Entry::TurnEnded { aborted: false }
            ])
        );
    }

    #[test]
    fn live_push_equals_resume() {
        let events = vec![
            user("go"),
            assistant(vec![call("c1")], StopReason::ToolUse),
            interrupt(),
            result("c1"),
            ended(TurnEndReason::Aborted),
        ];
        let mut live = History::default();
        for ev in &events {
            live.push(ev);
        }
        assert_eq!(live, History::from_events(&events));
    }

    #[test]
    fn failed_turn_text_merges_with_next_message() {
        let msgs = messages(&[
            user("a"),
            Event::Error {
                message: "x".into(),
                retryable: true,
            },
            ended(TurnEndReason::Failed),
            user("b"),
            assistant(vec![text("r")], StopReason::EndTurn),
            ended(TurnEndReason::Completed),
        ]);
        assert_eq!(
            msgs,
            vec![
                Message {
                    role: Role::User,
                    content: vec![text("a"), text("b")]
                },
                Message::assistant(vec![text("r")])
            ]
        );
    }

    #[test]
    fn results_in_call_order_then_steer_text() {
        let msgs = messages(&[
            user("go"),
            assistant(vec![call("c1"), call("c2")], StopReason::ToolUse),
            user("steer!"),
            result("c2"), // arrived first
            result("c1"),
            assistant(vec![text("done")], StopReason::EndTurn),
            ended(TurnEndReason::Completed),
            user("next"),
        ]);
        let roles: Vec<_> = msgs.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![
                Role::User,
                Role::Assistant,
                Role::User,
                Role::Assistant,
                Role::User
            ]
        );
        assert_eq!(
            msgs[2].content,
            vec![
                ContentBlock::ToolResult {
                    call_id: "c1".into(),
                    content: "out c1".into(),
                    is_error: false
                },
                ContentBlock::ToolResult {
                    call_id: "c2".into(),
                    content: "out c2".into(),
                    is_error: false
                },
                text("steer!"),
            ]
        );
        assert_eq!(msgs[4], Message::user("next"));
    }

    #[test]
    fn aborted_turn_marker_lands_in_next_user_message() {
        let msgs = messages(&[
            user("go"),
            assistant(vec![call("c1")], StopReason::ToolUse),
            interrupt(),
            Event::ToolResult {
                call_id: "c1".into(),
                name: "Bash".into(),
                content: "aborted".into(),
                is_error: true,
                meta: Value::Null,
            },
            ended(TurnEndReason::Aborted),
            user("try again"),
        ]);
        assert_eq!(msgs.len(), 3);
        assert_eq!(
            msgs[2].content,
            vec![
                ContentBlock::ToolResult {
                    call_id: "c1".into(),
                    content: "aborted".into(),
                    is_error: true
                },
                text(ABORT_MARKER),
                text("try again"),
            ]
        );
    }

    #[test]
    fn steer_during_text_reply_lands_between_replies() {
        let msgs = messages(&[
            user("a"),
            user("b"),
            assistant(vec![text("r1")], StopReason::EndTurn),
            assistant(vec![text("r2")], StopReason::EndTurn),
        ]);
        assert_eq!(
            msgs,
            vec![
                Message::user("a"),
                Message::assistant(vec![text("r1")]),
                Message::user("b"),
                Message::assistant(vec![text("r2")]),
            ]
        );
    }
}
