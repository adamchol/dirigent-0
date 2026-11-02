//! The only state the loop keeps across turns: what the next model call
//! needs. Built from handled events with their ids (see `dirigent_protocol::fold`), live
//! (`push`) or on resume (`from_events`). A `Compacted` event replaces what came before the
//! event it keeps from with its summary.

use std::collections::BTreeSet;

use crate::provider::{ContentBlock, Message, Role, Usage};
use dirigent_protocol::{Event, TurnEndReason};

pub const ABORT_MARKER: &str =
    "The user interrupted the previous turn. Tools that were running may have partially executed.";

const SUMMARY_PREAMBLE: &str = "This session was compacted: the summary below replaces the conversation before it. What follows the summary is kept as it was.";

/// For estimating tokens from text.
pub const CHARS_PER_TOKEN: u64 = 4;

/// The files a dropped part of the conversation touched, by absolute path.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Files {
    pub read: BTreeSet<String>,
    pub modified: BTreeSet<String>,
}

impl Files {
    fn add(&mut self, entry: &Entry) {
        match entry {
            Entry::Assistant { content, .. } => {
                for b in content {
                    let ContentBlock::ToolCall { name, input, .. } = b else {
                        continue;
                    };
                    let Some(path) = input.get("file_path").and_then(|p| p.as_str()) else {
                        continue;
                    };
                    match name.as_str() {
                        "Read" => self.read.insert(path.to_string()),
                        "Write" | "Edit" => self.modified.insert(path.to_string()),
                        _ => false,
                    };
                }
            }
            Entry::Summary { files, .. } => {
                self.read.extend(files.read.iter().cloned());
                self.modified.extend(files.modified.iter().cloned());
            }
            Entry::User { .. } | Entry::ToolResult { .. } | Entry::TurnEnded { .. } => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    User {
        text: String,
    },
    Assistant {
        content: Vec<ContentBlock>,
        usage: Usage,
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
    /// What a compaction left of the conversation before the entry it kept from.
    Summary {
        text: String,
        files: Files,
    },
}

impl Entry {
    /// The single conversion from events, used live and on resume.
    pub fn from_event(ev: &Event) -> Option<Entry> {
        match ev {
            Event::UserMessage { text, .. } => Some(Entry::User { text: text.clone() }),
            Event::AssistantMessage { content, usage, .. } => Some(Entry::Assistant {
                content: content.clone(),
                usage: *usage,
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
            Event::TurnEnded { reason, .. } => Some(Entry::TurnEnded {
                aborted: *reason == TurnEndReason::Aborted,
            }),
            // Applied by `History::push`.
            Event::Compacted { .. } => None,
            Event::PlayerAttached { .. }
            | Event::UserInterrupt { .. }
            | Event::TurnStarted { .. }
            | Event::Steered { .. }
            | Event::ModelDelta { .. }
            | Event::ModelCancelled
            | Event::Error { .. }
            | Event::CompactionFailed { .. } => None,
        }
    }

    /// The size of what the model sees of it.
    fn chars(&self) -> u64 {
        let n = match self {
            Entry::User { text } => text.len(),
            Entry::Assistant { content, .. } => content
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { text } | ContentBlock::Thinking { text, .. } => text.len(),
                    ContentBlock::ToolCall { name, input, .. } => {
                        name.len() + input.to_string().len()
                    }
                    ContentBlock::ToolResult { content, .. } => content.len(),
                })
                .sum(),
            Entry::ToolResult { content, .. } => content.len(),
            Entry::TurnEnded { aborted } => {
                if *aborted {
                    ABORT_MARKER.len()
                } else {
                    0
                }
            }
            Entry::Summary { text, files } => render_summary(text, files).len(),
        };
        n as u64
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct History {
    /// With the ids of the events they came from.
    entries: Vec<(String, Entry)>,
    /// Entries from here on came after the last compaction, so their usage measures the
    /// context as it is now.
    measured_from: usize,
}

impl History {
    pub fn from_events(events: &[(String, Event)]) -> Self {
        let mut h = History::default();
        for (id, ev) in events {
            h.push(id, ev);
        }
        h
    }

    pub fn push(&mut self, id: &str, ev: &Event) {
        if let Event::Compacted {
            summary,
            first_kept,
            ..
        } = ev
        {
            self.compact(id, summary, first_kept);
        } else if let Some(entry) = Entry::from_event(ev) {
            self.entries.push((id.to_string(), entry));
        }
    }

    /// Replace the entries before `first_kept` with the summary. The files they touched are
    /// derived here, the same way live and on resume, and stay as they are until the next
    /// compaction so the summary message keeps its bytes.
    fn compact(&mut self, id: &str, summary: &str, first_kept: &str) {
        let keep = self
            .entries
            .iter()
            .position(|(id, _)| id == first_kept)
            .unwrap_or(0);
        let mut files = Files::default();
        for (_, entry) in self.entries.drain(..keep) {
            files.add(&entry);
        }
        let summary = Entry::Summary {
            text: summary.to_string(),
            files,
        };
        self.entries.insert(0, (id.to_string(), summary));
        self.measured_from = self.entries.len();
    }

    /// Estimated tokens of the next call's context: the last reply's usage plus the
    /// characters since it, or, with no reply since the last compaction, the characters of
    /// everything with `fixed` for the system prompt and tools.
    pub fn tokens(&self, fixed: u64) -> u64 {
        let measured = self.entries[self.measured_from.min(self.entries.len())..]
            .iter()
            .rposition(|(_, e)| matches!(e, Entry::Assistant { usage, .. } if total(usage) > 0))
            .map(|i| self.measured_from + i);
        match measured {
            Some(i) => {
                let Entry::Assistant { usage, .. } = &self.entries[i].1 else {
                    unreachable!("found as an assistant entry")
                };
                total(usage) + chars(&self.entries[i + 1..]) / CHARS_PER_TOKEN
            }
            None => fixed + chars(&self.entries) / CHARS_PER_TOKEN,
        }
    }

    /// Where a compaction would keep from, so that about `keep` tokens stay as they are: the
    /// id of that entry, and the estimated tokens of what comes before it. Only a user message
    /// or a reply starts what is kept, so a tool call is never parted from its results. `None`
    /// when nothing would be dropped.
    pub fn cut(&self, keep: u64) -> Option<(String, u64)> {
        let mut i = self.entries.len();
        let mut tail = 0;
        while i > 0 && tail < keep * CHARS_PER_TOKEN {
            i -= 1;
            tail += self.entries[i].1.chars();
        }
        let j = (1..=i.min(self.entries.len().saturating_sub(1)))
            .rev()
            .find(|&j| {
                matches!(
                    self.entries[j].1,
                    Entry::User { .. } | Entry::Assistant { .. }
                )
            })?;
        Some((
            self.entries[j].0.clone(),
            chars(&self.entries[..j]) / CHARS_PER_TOKEN,
        ))
    }

    /// The model's conversation. A user message sent while a model call was
    /// in flight lands after that call's reply; tool results open the next
    /// user message, in call order. A summary opens it; when what it kept starts
    /// with a reply, it was cut mid-turn and the summary stands for the turn's start.
    pub fn messages(&self) -> Vec<Message> {
        let mut out: Vec<Message> = Vec::new();
        let mut results: Vec<ContentBlock> = Vec::new();
        let mut texts: Vec<String> = Vec::new();
        let mut held: Vec<String> = Vec::new();
        let mut calls: Vec<String> = Vec::new();
        let mut in_turn = false;
        let mut calling = false;

        for (i, (_, entry)) in self.entries.iter().enumerate() {
            match entry {
                Entry::Summary { text, files } => {
                    texts.push(render_summary(text, files));
                    in_turn = matches!(self.entries.get(i + 1), Some((_, Entry::Assistant { .. })));
                    calling = in_turn;
                }
                Entry::User { text } if !in_turn => {
                    in_turn = true;
                    calling = true;
                    texts.push(text.clone());
                }
                Entry::User { text } if calling => held.push(text.clone()),
                Entry::User { text } => texts.push(text.clone()),
                Entry::Assistant { content, .. } => {
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

fn total(u: &Usage) -> u64 {
    u.input + u.cache_read + u.cache_write + u.output
}

fn chars(entries: &[(String, Entry)]) -> u64 {
    entries.iter().map(|(_, e)| e.chars()).sum()
}

fn render_summary(text: &str, files: &Files) -> String {
    let mut out = format!(
        "{SUMMARY_PREAMBLE}\n\n<summary>\n{}\n</summary>",
        text.trim()
    );
    let read: Vec<&str> = files
        .read
        .difference(&files.modified)
        .map(String::as_str)
        .collect();
    if !read.is_empty() {
        out.push_str(&format!("\n\nFiles read: {}", read.join(", ")));
    }
    if !files.modified.is_empty() {
        let modified: Vec<&str> = files.modified.iter().map(String::as_str).collect();
        out.push_str(&format!("\n\nFiles modified: {}", modified.join(", ")));
    }
    out
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
    use crate::provider::StopReason;
    use serde_json::{json, Value};

    fn user(text: &str) -> Event {
        Event::UserMessage { text: text.into() }
    }

    fn interrupt() -> Event {
        Event::UserInterrupt { turn: "t1".into() }
    }

    fn assistant(content: Vec<ContentBlock>, stop: StopReason) -> Event {
        Event::AssistantMessage {
            content,
            stop_reason: stop,
            usage: Usage::default(),
        }
    }

    fn call(id: &str) -> ContentBlock {
        read_call(id, "/a")
    }

    fn read_call(id: &str, path: &str) -> ContentBlock {
        tool_call(id, "Read", path)
    }

    fn tool_call(id: &str, name: &str, path: &str) -> ContentBlock {
        ContentBlock::ToolCall {
            id: id.into(),
            name: name.into(),
            input: json!({ "file_path": path }),
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
        Event::TurnEnded {
            reason,
            interrupt: None,
        }
    }

    fn compacted(summary: &str, first_kept: &str) -> Event {
        Event::Compacted {
            summary: summary.into(),
            first_kept: first_kept.into(),
            usage: Usage::default(),
        }
    }

    fn text(t: &str) -> ContentBlock {
        ContentBlock::Text { text: t.into() }
    }

    /// The events with ids `e0`, `e1`, … by position.
    fn logged(events: &[Event]) -> Vec<(String, Event)> {
        events
            .iter()
            .enumerate()
            .map(|(i, e)| (format!("e{i}"), e.clone()))
            .collect()
    }

    fn history(events: &[Event]) -> History {
        History::from_events(&logged(events))
    }

    fn messages(events: &[Event]) -> Vec<Message> {
        history(events).messages()
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
                retry_in_ms: None,
                compact: false,
            },
            Event::CompactionFailed {
                message: "x".into(),
            },
            ended(TurnEndReason::Failed),
        ];
        assert_eq!(
            history(&events).entries,
            vec![
                ("e0".into(), Entry::User { text: "a".into() }),
                ("e6".into(), Entry::TurnEnded { aborted: false })
            ]
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
            user("again"),
            compacted("S", "e5"),
            assistant(vec![text("r")], StopReason::EndTurn),
        ];
        let mut live = History::default();
        for (id, ev) in &logged(&events) {
            live.push(id, ev);
        }
        assert_eq!(live, history(&events));
    }

    #[test]
    fn failed_turn_text_merges_with_next_message() {
        let msgs = messages(&[
            user("a"),
            Event::Error {
                message: "x".into(),
                retry_in_ms: None,
                compact: false,
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

    #[test]
    fn a_compaction_replaces_what_came_before_the_kept_message_with_its_summary() {
        let msgs = messages(&[
            user("a"),
            assistant(vec![read_call("c1", "/x")], StopReason::ToolUse),
            result("c1"),
            assistant(vec![text("r1")], StopReason::EndTurn),
            ended(TurnEndReason::Completed),
            user("b"),
            compacted("S", "e5"),
            assistant(vec![text("r2")], StopReason::EndTurn),
        ]);
        assert_eq!(
            msgs,
            vec![
                Message {
                    role: Role::User,
                    content: vec![
                        text(&format!(
                            "{SUMMARY_PREAMBLE}\n\n<summary>\nS\n</summary>\n\nFiles read: /x"
                        )),
                        text("b"),
                    ]
                },
                Message::assistant(vec![text("r2")]),
            ]
        );
    }

    #[test]
    fn a_cut_mid_turn_keeps_from_a_reply_and_the_summary_opens_the_turn() {
        let msgs = messages(&[
            user("a"),
            assistant(vec![call("c1")], StopReason::ToolUse),
            result("c1"),
            assistant(vec![call("c2")], StopReason::ToolUse),
            user("steer"),
            result("c2"),
            compacted("S", "e3"),
            assistant(vec![text("done")], StopReason::EndTurn),
        ]);
        let roles: Vec<_> = msgs.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![Role::User, Role::Assistant, Role::User, Role::Assistant]
        );
        assert!(msgs[0].text().contains("<summary>\nS\n</summary>"));
        assert_eq!(msgs[1], Message::assistant(vec![call("c2")]));
        assert_eq!(
            msgs[2].content,
            vec![
                ContentBlock::ToolResult {
                    call_id: "c2".into(),
                    content: "out c2".into(),
                    is_error: false
                },
                text("steer"),
            ]
        );
    }

    #[test]
    fn the_files_of_every_compaction_add_up_and_a_modified_file_is_not_listed_as_read() {
        let msgs = messages(&[
            user("a"),
            assistant(
                vec![read_call("c1", "/r"), read_call("c2", "/w")],
                StopReason::ToolUse,
            ),
            result("c1"),
            result("c2"),
            user("b"),
            compacted("S1", "e4"),
            assistant(vec![tool_call("c3", "Edit", "/w")], StopReason::ToolUse),
            result("c3"),
            user("c"),
            compacted("S2", "e8"),
        ]);
        let ContentBlock::Text { text: first } = &msgs[0].content[0] else {
            panic!("the summary opens the first message")
        };
        assert!(first.contains("<summary>\nS2\n</summary>"));
        assert!(!first.contains("S1"));
        assert!(first.ends_with("\n\nFiles read: /r\n\nFiles modified: /w"));
    }

    #[test]
    fn tokens_are_the_last_usage_plus_what_came_since_and_stale_usage_is_ignored() {
        let mut h = history(&[user("a")]);
        assert_eq!(h.tokens(100), 100); // 1 char rounds down
        let reply = Event::AssistantMessage {
            content: vec![text("r")],
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input: 1000,
                output: 10,
                cache_read: 500,
                cache_write: 0,
            },
        };
        h.push("e1", &reply);
        h.push("e2", &user(&"x".repeat(400)));
        assert_eq!(h.tokens(100), 1510 + 100);
        // The kept reply's usage measured the context before the compaction.
        h.push("e3", &compacted(&"s".repeat(40), "e1"));
        let expected = 100 + (chars(&h.entries) / CHARS_PER_TOKEN);
        assert_eq!(h.tokens(100), expected);
        assert!(expected < 1510);
    }

    #[test]
    fn the_cut_keeps_about_the_asked_tokens_from_a_message_or_reply() {
        let big = "y".repeat(4000);
        let h = history(&[
            user(&"a".repeat(400)),
            assistant(vec![call("c1")], StopReason::ToolUse),
            Event::ToolResult {
                call_id: "c1".into(),
                name: "Read".into(),
                content: big,
                is_error: false,
                meta: Value::Null,
            },
        ]);
        // The result alone covers 1000 tokens; it is kept with its call.
        assert_eq!(h.cut(1000), Some(("e1".into(), 100)));
        // Keeping more than there is drops nothing.
        assert_eq!(h.cut(10_000), None);
        assert_eq!(history(&[user("only")]).cut(0), None);
    }
}
