//! Codex's sessions: `~/.codex/sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl` (or under
//! `$CODEX_HOME`), one `{timestamp, type, payload}` per line. The user's messages are its
//! `user_message` events (the `user` response items have the injected context besides); a
//! reply is the model's items in a row (reasoning, messages, tool calls), its usage the
//! `token_count` after it; `task_complete` and `turn_aborted` end turns.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::Context;
use dirigent_protocol::{ContentBlock, Event, StopReason, TurnEndReason, Usage};
use serde::Deserialize;
use serde_json::Value;

use super::{Conversation, Log};

#[derive(Deserialize)]
struct Line {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    payload: Value,
}

/// Codex's own directory: `$CODEX_HOME`, or `~/.codex`.
fn home() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").context("HOME isn't set")?;
    Ok(PathBuf::from(home).join(".codex"))
}

/// Every rollout under `dir`.
fn rollouts(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rollouts(&path, found);
        } else if path.extension().is_some_and(|e| e == "jsonl")
            && path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("rollout-"))
        {
            found.push(path);
        }
    }
}

/// The rollout of the session `session` (its id, or a path), or the last one of the current
/// directory.
pub fn find(session: Option<&str>) -> anyhow::Result<PathBuf> {
    if let Some(s) = session {
        let path = PathBuf::from(s);
        if path.extension().is_some_and(|e| e == "jsonl") && path.is_file() {
            return Ok(path);
        }
    }
    let sessions = home()?.join("sessions");
    let mut found = Vec::new();
    rollouts(&sessions, &mut found);
    // named by when they began
    found.sort_by_key(|p| p.file_name().map(|n| n.to_os_string()));
    if let Some(s) = session {
        return found
            .into_iter()
            .find(|p| p.to_string_lossy().ends_with(&format!("{s}.jsonl")))
            .with_context(|| format!("no Codex session {s} in {}", sessions.display()));
    }
    let here = std::env::current_dir()?;
    found
        .into_iter()
        .rev()
        .find(|p| meta_cwd(p).is_some_and(|cwd| Path::new(&cwd) == here))
        .with_context(|| {
            format!(
                "no Codex sessions of {} in {}",
                here.display(),
                sessions.display()
            )
        })
}

/// The directory a rollout's session worked in, from its first line.
fn meta_cwd(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    std::io::BufReader::new(file).read_line(&mut first).ok()?;
    let line: Line = serde_json::from_str(&first).ok()?;
    (line.kind == "session_meta")
        .then(|| line.payload["cwd"].as_str().map(str::to_string))
        .flatten()
}

pub fn read(path: &Path) -> anyhow::Result<Conversation> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut lines = Vec::new();
    for line in std::io::BufReader::new(file).lines() {
        let line = line.with_context(|| format!("reading {}", path.display()))?;
        if let Ok(l) = serde_json::from_str::<Line>(&line) {
            lines.push(l);
        }
    }
    Ok(convert(&lines, Log::default()))
}

fn convert(lines: &[Line], log: Log) -> Conversation {
    let mut conv = Conversation::default();
    let mut read = Reading {
        log,
        reply: Vec::new(),
        names: HashMap::new(),
    };
    for l in lines {
        if let Some(t) = &l.timestamp {
            if conv.first.is_empty() {
                conv.first = t.clone();
            }
            conv.last = t.clone();
        }
        let p = &l.payload;
        match l.kind.as_str() {
            "session_meta" => {
                conv.session = p["id"].as_str().unwrap_or_default().into();
                conv.cwd = PathBuf::from(p["cwd"].as_str().unwrap_or_default());
            }
            "turn_context" => {
                if let Some(model) = p["model"].as_str() {
                    conv.model = Some(model.into());
                }
            }
            "response_item" => read.item(p),
            "event_msg" => read.event(p),
            "compacted" => {
                read.flush(Usage::default());
                let summary = p["message"].as_str().unwrap_or_default().to_string();
                // Codex keeps no reply of what it compacts
                read.log.compacted(summary, None);
            }
            _ => {}
        }
    }
    read.flush(Usage::default());
    read.log.finish();
    conv.events = read.log.events;
    conv
}

struct Reading {
    log: Log,
    /// The reply's blocks so far.
    reply: Vec<ContentBlock>,
    /// Tool calls' names, by id.
    names: HashMap<String, String>,
}

impl Reading {
    fn item(&mut self, p: &Value) {
        let str = |k: &str| p[k].as_str().unwrap_or_default().to_string();
        match p["type"].as_str() {
            Some("message") if p["role"] == "assistant" => {
                let text = texts(&p["content"]);
                if !text.is_empty() {
                    self.reply.push(ContentBlock::Text { text });
                }
            }
            // kept to read, its encrypted content never sent again
            Some("reasoning") => {
                let text = texts(&p["summary"]);
                if !text.is_empty() {
                    self.reply.push(ContentBlock::Thinking {
                        text,
                        provider_data: Value::Null,
                    });
                }
            }
            Some("function_call") => {
                let arguments = str("arguments");
                let input = serde_json::from_str(&arguments).unwrap_or(Value::String(arguments));
                self.call(str("call_id"), str("name"), input);
            }
            Some("custom_tool_call") => {
                let input = serde_json::json!({ "input": p["input"] });
                self.call(str("call_id"), str("name"), input);
            }
            Some("local_shell_call") => {
                let id = p["call_id"]
                    .as_str()
                    .or(p["id"].as_str())
                    .unwrap_or_default();
                self.call(id.into(), "local_shell".into(), p["action"].clone());
            }
            Some("web_search_call") => {
                let query = p["action"]["query"].as_str().unwrap_or_default();
                self.reply.push(ContentBlock::Text {
                    text: format!("[web search: {query}]"),
                });
            }
            Some("function_call_output") | Some("custom_tool_call_output") => {
                self.flush(Usage::default());
                let call_id = str("call_id");
                let output = match &p["output"] {
                    Value::String(s) => s.clone(),
                    Value::Object(o) => o
                        .get("content")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    _ => String::new(),
                };
                let failed = p["output"]["success"] == Value::Bool(false);
                self.log.push(Event::ToolResult {
                    name: self.names.get(&call_id).cloned().unwrap_or_default(),
                    call_id,
                    content: output,
                    is_error: failed,
                    meta: Value::Null,
                });
            }
            _ => {}
        }
    }

    fn call(&mut self, id: String, name: String, input: Value) {
        self.names.insert(id.clone(), name.clone());
        self.reply.push(ContentBlock::ToolCall { id, name, input });
    }

    fn event(&mut self, p: &Value) {
        match p["type"].as_str() {
            Some("user_message") => {
                self.flush(Usage::default());
                let text = p["message"].as_str().unwrap_or_default().to_string();
                if !text.trim().is_empty() {
                    self.log.input(text);
                }
            }
            Some("token_count") => {
                let u = &p["info"]["last_token_usage"];
                let n = |k: &str| u[k].as_u64().unwrap_or(0);
                let cached = n("cached_input_tokens");
                self.flush(Usage {
                    input: n("input_tokens").saturating_sub(cached),
                    output: n("output_tokens"),
                    cache_read: cached,
                    cache_write: 0,
                });
            }
            Some("task_complete") => {
                self.flush(Usage::default());
                self.log.end(TurnEndReason::Completed);
            }
            Some("turn_aborted") => {
                self.flush(Usage::default());
                self.log.end(TurnEndReason::Aborted);
            }
            _ => {}
        }
    }

    /// The reply read so far, with its usage.
    fn flush(&mut self, usage: Usage) {
        if self.reply.is_empty() {
            return;
        }
        let content = std::mem::take(&mut self.reply);
        let stop_reason = match content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolCall { .. }))
        {
            true => StopReason::ToolUse,
            false => StopReason::EndTurn,
        };
        self.log.push(Event::AssistantMessage {
            content,
            stop_reason,
            usage,
        });
    }
}

/// The text of a message's content (or a reasoning's summary) items.
fn texts(items: &Value) -> String {
    let parts: Vec<&str> = items
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|i| i["text"].as_str())
        .collect();
    parts.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::tests::{kinds, log};
    use serde_json::json;

    fn line(kind: &str, payload: Value) -> Line {
        Line {
            timestamp: Some("2026-11-01T00:00:00Z".into()),
            kind: kind.into(),
            payload,
        }
    }

    fn item(payload: Value) -> Line {
        line("response_item", payload)
    }

    fn event(payload: Value) -> Line {
        line("event_msg", payload)
    }

    #[test]
    fn a_rollout_reads_as_turns_with_replies_and_results() {
        let conv = convert(
            &[
                line(
                    "session_meta",
                    json!({ "id": "cx", "cwd": "/p", "cli_version": "0.60.0" }),
                ),
                line(
                    "turn_context",
                    json!({ "cwd": "/p", "model": "gpt-6-codex" }),
                ),
                // the injected context, and the user's message as an item: left out
                item(json!({ "type": "message", "role": "user",
                             "content": [{ "type": "input_text", "text": "<environment_context>" }] })),
                event(json!({ "type": "task_started" })),
                item(json!({ "type": "message", "role": "user",
                             "content": [{ "type": "input_text", "text": "fix it" }] })),
                event(json!({ "type": "user_message", "message": "fix it" })),
                item(json!({ "type": "reasoning", "encrypted_content": "x",
                             "summary": [{ "type": "summary_text", "text": "look" }] })),
                item(
                    json!({ "type": "function_call", "name": "shell", "call_id": "c1",
                             "arguments": "{\"command\":[\"bash\",\"-lc\",\"ls\"]}" }),
                ),
                event(
                    json!({ "type": "token_count", "info": { "last_token_usage": {
                    "input_tokens": 10, "cached_input_tokens": 4, "output_tokens": 2 } } }),
                ),
                item(json!({ "type": "function_call_output", "call_id": "c1", "output": "a b" })),
                item(
                    json!({ "type": "custom_tool_call", "name": "apply_patch", "call_id": "c2",
                             "input": "*** Begin Patch" }),
                ),
                item(json!({ "type": "custom_tool_call_output", "call_id": "c2",
                             "output": { "content": "bad", "success": false } })),
                item(json!({ "type": "message", "role": "assistant",
                             "content": [{ "type": "output_text", "text": "done" }] })),
                event(json!({ "type": "task_complete" })),
                event(json!({ "type": "user_message", "message": "more" })),
                line("compacted", json!({ "message": "SUMMARY" })),
                event(json!({ "type": "turn_aborted", "reason": "interrupted" })),
            ],
            log(),
        );
        assert_eq!(
            kinds(&conv.events),
            [
                "user:fix it",
                "turn:e0",
                "reply:ToolUse:[thinking:look,call:shell:c1]",
                "result:shell:c1:false:a b",
                "reply:ToolUse:[call:apply_patch:c2]",
                "result:apply_patch:c2:true:bad",
                "reply:EndTurn:[text:done]",
                "end:Completed",
                "user:more",
                "turn:e8",
                "compacted:e10:SUMMARY",
                "end:Aborted",
            ]
        );
        let Event::AssistantMessage { content, usage, .. } = &conv.events[2].1 else {
            panic!()
        };
        assert_eq!(
            *usage,
            Usage {
                input: 6,
                output: 2,
                cache_read: 4,
                cache_write: 0
            }
        );
        assert_eq!(
            content[1],
            ContentBlock::ToolCall {
                id: "c1".into(),
                name: "shell".into(),
                input: json!({ "command": ["bash", "-lc", "ls"] }),
            }
        );
        assert_eq!(conv.session, "cx");
        assert_eq!(conv.cwd, PathBuf::from("/p"));
        assert_eq!(conv.model.as_deref(), Some("gpt-6-codex"));
    }
}
