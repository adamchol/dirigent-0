//! Claude Code's sessions: `~/.claude/projects/<the directory, dashed>/<id>.jsonl` (or under
//! `$CLAUDE_CONFIG_DIR`), one entry per line. The conversation is the chain of entries from the
//! last one back through their parents (past a compaction, through its logical parent), with the
//! tool results that hang off it; a reply is split over lines, one per content block, sharing
//! its message's id. Attachments, reminders, meta entries and the API's captures are left out,
//! and so are subagents' own transcripts (their results are the parent's).

use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};

use anyhow::Context;
use dirigent_protocol::{ContentBlock, Event, TurnEndReason, Usage};
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::Value;

use super::{stop_reason, Conversation, Log};

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct Line {
    #[serde(rename = "type")]
    kind: String,
    uuid: Option<String>,
    parent_uuid: Option<String>,
    logical_parent_uuid: Option<String>,
    is_sidechain: bool,
    is_meta: bool,
    is_compact_summary: bool,
    is_api_error_message: bool,
    subtype: Option<String>,
    timestamp: Option<String>,
    cwd: Option<String>,
    session_id: Option<String>,
    prompt_source: Option<String>,
    origin: Option<Origin>,
    message: Option<Box<RawValue>>,
    compact_metadata: Option<CompactMetadata>,
    custom_title: Option<String>,
    ai_title: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Origin {
    kind: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct CompactMetadata {
    preserved_segment: Option<Segment>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
struct Segment {
    head_uuid: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Message {
    id: Option<String>,
    model: Option<String>,
    content: Content,
    stop_reason: Option<String>,
    usage: Option<ApiUsage>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Blocks(Vec<Value>),
}

impl Default for Content {
    fn default() -> Self {
        Content::Blocks(Vec::new())
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ApiUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
}

impl From<&ApiUsage> for Usage {
    fn from(u: &ApiUsage) -> Usage {
        Usage {
            input: u.input_tokens,
            output: u.output_tokens,
            cache_read: u.cache_read_input_tokens,
            cache_write: u.cache_creation_input_tokens,
        }
    }
}

/// Claude Code's own directory: `$CLAUDE_CONFIG_DIR`, or `~/.claude`.
fn home() -> anyhow::Result<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var_os("HOME").context("HOME isn't set")?;
    Ok(PathBuf::from(home).join(".claude"))
}

/// A project's directory under `projects/`: its path with every other character than a letter
/// or digit a dash.
fn dashed(dir: &Path) -> String {
    dir.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The transcript of the session `session` (its id, or a path), or the last one of the current
/// directory.
pub fn find(session: Option<&str>) -> anyhow::Result<PathBuf> {
    let projects = home()?.join("projects");
    if let Some(s) = session {
        let path = PathBuf::from(s);
        if path.extension().is_some_and(|e| e == "jsonl") && path.is_file() {
            return Ok(path);
        }
        for dir in std::fs::read_dir(&projects)
            .with_context(|| format!("reading {}", projects.display()))?
            .flatten()
        {
            let path = dir.path().join(format!("{s}.jsonl"));
            if path.is_file() {
                return Ok(path);
            }
        }
        anyhow::bail!("no Claude Code session {s} in {}", projects.display());
    }
    let here = std::env::current_dir()?;
    let dir = projects.join(dashed(&here));
    let last = std::fs::read_dir(&dir)
        .with_context(|| {
            format!(
                "no Claude Code sessions of {} ({})",
                here.display(),
                dir.display()
            )
        })?
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .max();
    match last {
        Some((_, path)) => Ok(path),
        None => anyhow::bail!(
            "no Claude Code sessions of {} ({})",
            here.display(),
            dir.display()
        ),
    }
}

pub fn read(path: &Path) -> anyhow::Result<Conversation> {
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut lines = Vec::new();
    for line in std::io::BufReader::new(file).lines() {
        let line = line.with_context(|| format!("reading {}", path.display()))?;
        // the API's captures, the biggest part, aren't the conversation
        if line.starts_with(r#"{"type":"api-request"#) {
            continue;
        }
        let Ok(l) = serde_json::from_str::<Line>(&line) else {
            continue;
        };
        // entries of the chain (attachments too, which link it), and the titles
        let titled = matches!(l.kind.as_str(), "custom-title" | "ai-title");
        if (l.uuid.is_some() || titled) && !l.is_sidechain {
            lines.push(l);
        }
    }
    let id = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    Ok(convert(&lines, id))
}

fn convert(lines: &[Line], id: String) -> Conversation {
    let title = lines
        .iter()
        .rev()
        .find_map(|l| l.custom_title.clone().map(|t| (t, true)))
        .or_else(|| {
            lines
                .iter()
                .rev()
                .find_map(|l| l.ai_title.clone().map(|t| (t, false)))
        });
    let chain = chain(lines);
    let mut conv = Conversation {
        session: lines
            .iter()
            .find_map(|l| l.session_id.clone())
            .unwrap_or(id),
        title,
        ..Conversation::default()
    };
    let mut read = Reading {
        log: Log::default(),
        reply: None,
        results: Vec::new(),
        names: HashMap::new(),
        starts: HashMap::new(),
        kept: None,
    };
    for (i, l) in lines.iter().enumerate() {
        if !chain.contains(&i) {
            continue;
        }
        if let Some(t) = &l.timestamp {
            if conv.first.is_empty() {
                conv.first = t.clone();
            }
            conv.last = t.clone();
        }
        if conv.cwd.as_os_str().is_empty() {
            if let Some(cwd) = &l.cwd {
                conv.cwd = PathBuf::from(cwd);
            }
        }
        if let Some(model) = read.line(l) {
            conv.model = Some(model);
        }
    }
    read.flush();
    read.log.finish();
    conv.events = read.log.events;
    conv
}

/// The lines of the conversation, by index: from the last entry back through parents, and those
/// hanging off them that it needs (a reply's other blocks, tool results).
fn chain(lines: &[Line]) -> HashSet<usize> {
    let mut by_uuid = HashMap::new();
    for (i, l) in lines.iter().enumerate() {
        if let Some(u) = &l.uuid {
            by_uuid.entry(u.as_str()).or_insert(i);
        }
    }
    let mut on = HashSet::new();
    let leaf = lines
        .iter()
        .rposition(|l| matches!(l.kind.as_str(), "user" | "assistant"));
    let mut at = leaf;
    while let Some(i) = at {
        if !on.insert(i) {
            break;
        }
        let l = &lines[i];
        let parent = l.parent_uuid.as_ref().or(l.logical_parent_uuid.as_ref());
        at = parent.and_then(|p| by_uuid.get(p.as_str()).copied());
    }
    // off it: a call's result, and another block of the same reply (not one answered anew)
    let id = |i: usize| message(&lines[i]).and_then(|m| m.id);
    let hanging: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(i, l)| {
            let Some(parent) = l.parent_uuid.as_deref().and_then(|p| by_uuid.get(p)) else {
                return false;
            };
            !on.contains(i)
                && on.contains(parent)
                && match l.kind.as_str() {
                    "user" => has_results(l),
                    "assistant" => lines[*parent].kind == "assistant" && id(*i) == id(*parent),
                    _ => false,
                }
        })
        .map(|(i, _)| i)
        .collect();
    on.extend(hanging);
    on
}

fn message(l: &Line) -> Option<Message> {
    serde_json::from_str(l.message.as_ref()?.get()).ok()
}

fn has_results(l: &Line) -> bool {
    matches!(message(l), Some(Message { content: Content::Blocks(b), .. })
        if b.iter().any(|b| b["type"] == "tool_result"))
}

/// The conversation as it's read: the reply whose blocks are still coming, and the results
/// that came meanwhile.
struct Reading {
    log: Log,
    /// The reply's message id, blocks, stop reason and usage.
    reply: Option<(String, Vec<ContentBlock>, Option<String>, Usage)>,
    results: Vec<Event>,
    /// Tool calls' names, by id.
    names: HashMap<String, String>,
    /// Where each line's events begin.
    starts: HashMap<String, usize>,
    /// The first event a compaction keeps, until its summary comes.
    kept: Option<Option<String>>,
}

impl Reading {
    /// Read the line `l`; the model of the reply it's of, if it's one.
    fn line(&mut self, l: &Line) -> Option<String> {
        let msg = message(l);
        let joins = match (&self.reply, l.kind.as_str(), &msg) {
            (Some((id, ..)), "assistant", Some(m)) => m.id.as_ref() == Some(id),
            (Some(_), "user", _) => has_results(l),
            _ => false,
        };
        if !joins {
            self.flush();
        }
        if let Some(u) = &l.uuid {
            self.starts.insert(u.clone(), self.log.events.len());
        }
        match l.kind.as_str() {
            "system" => match l.subtype.as_deref() {
                Some("compact_boundary") => {
                    let head = l
                        .compact_metadata
                        .as_ref()
                        .and_then(|m| m.preserved_segment.as_ref())
                        .and_then(|s| s.head_uuid.as_ref());
                    // the head's first event (a turn it ended isn't its)
                    let kept = head.and_then(|h| self.starts.get(h)).and_then(|i| {
                        self.log.events[*i..]
                            .iter()
                            .find(|(_, e)| !matches!(e, Event::TurnEnded { .. }))
                            .map(|(id, _)| id.clone())
                    });
                    self.kept = Some(kept);
                }
                Some("turn_duration") => self.log.end(TurnEndReason::Completed),
                _ => {}
            },
            "user" if l.is_meta => {}
            "user" => self.user(l, msg?),
            "assistant" if l.is_api_error_message => {}
            "assistant" => return self.assistant(msg?),
            _ => {}
        }
        None
    }

    fn user(&mut self, l: &Line, msg: Message) {
        let blocks = match msg.content {
            Content::Text(text) => vec![serde_json::json!({ "type": "text", "text": text })],
            Content::Blocks(b) => b,
        };
        if l.is_compact_summary {
            let summary = texts(&blocks);
            let kept = self.kept.take().flatten();
            self.log.compacted(summary, kept);
            return;
        }
        let mut text = Vec::new();
        for b in &blocks {
            match b["type"].as_str() {
                Some("tool_result") => {
                    let call_id = b["tool_use_id"].as_str().unwrap_or_default().to_string();
                    let result = Event::ToolResult {
                        name: self.names.get(&call_id).cloned().unwrap_or_default(),
                        call_id,
                        content: result_text(&b["content"]),
                        is_error: b["is_error"].as_bool().unwrap_or(false),
                        meta: Value::Null,
                    };
                    match self.reply {
                        Some(_) => self.results.push(result),
                        None => {
                            self.log.push(result);
                        }
                    }
                }
                _ => text.push(b.clone()),
            }
        }
        if text.is_empty() {
            return;
        }
        let text = texts(&text);
        let trimmed = text.trim_start();
        let origin = l.origin.as_ref().and_then(|o| o.kind.as_deref());
        // what came while a turn ran (a background task's result, a message queued) joins it
        let joins = origin == Some("task-notification")
            || trimmed.starts_with("<task-notification>")
            || l.prompt_source.as_deref() == Some("queued");
        if trimmed.starts_with("[Request interrupted by user") {
            self.log.end(TurnEndReason::Aborted);
        } else if trimmed.starts_with("<local-command-")
            || trimmed.starts_with("<system-reminder>")
            || trimmed.is_empty()
        {
        } else if let Some(command) = command(trimmed) {
            self.log.prompt(command);
        } else if joins {
            self.log.input(text);
        } else {
            self.log.prompt(text);
        }
    }

    fn assistant(&mut self, msg: Message) -> Option<String> {
        let Content::Blocks(blocks) = &msg.content else {
            return None;
        };
        let id = msg.id.clone().unwrap_or_default();
        let reply = self
            .reply
            .get_or_insert_with(|| (id, Vec::new(), None, Usage::default()));
        for b in blocks {
            match b["type"].as_str() {
                Some("text") => {
                    let text = b["text"].as_str().unwrap_or_default();
                    if !text.is_empty() {
                        reply.1.push(ContentBlock::Text { text: text.into() });
                    }
                }
                // kept to read, without its signature: it's bound to the conversation it was
                // thought in, which this isn't
                Some("thinking") => {
                    let text = b["thinking"].as_str().unwrap_or_default();
                    if !text.is_empty() {
                        reply.1.push(ContentBlock::Thinking {
                            text: text.into(),
                            provider_data: Value::Null,
                        });
                    }
                }
                Some("tool_use") => {
                    let id = b["id"].as_str().unwrap_or_default().to_string();
                    let name = b["name"].as_str().unwrap_or_default().to_string();
                    self.names.insert(id.clone(), name.clone());
                    reply.1.push(ContentBlock::ToolCall {
                        id,
                        name,
                        input: b["input"].clone(),
                    });
                }
                _ => {}
            }
        }
        if msg.stop_reason.is_some() {
            reply.2 = msg.stop_reason.clone();
        }
        if let Some(u) = &msg.usage {
            reply.3 = u.into();
        }
        msg.model.filter(|m| !m.starts_with('<'))
    }

    /// The reply read so far, and the results that came with it.
    fn flush(&mut self) {
        if let Some((_, content, stop, usage)) = self.reply.take() {
            if !content.is_empty() {
                let stop_reason = stop_reason(stop.as_deref(), &content);
                self.log.push(Event::AssistantMessage {
                    content,
                    stop_reason,
                    usage,
                });
            }
        }
        for result in std::mem::take(&mut self.results) {
            self.log.push(result);
        }
    }
}

/// A slash command's line (`<command-name>/x</command-name>…<command-args>a</command-args>`) as
/// it was typed.
fn command(text: &str) -> Option<String> {
    let tag = |name: &str| {
        let open = format!("<{name}>");
        let start = text.find(&open)? + open.len();
        let end = start + text[start..].find(&format!("</{name}>"))?;
        Some(text[start..end].trim().to_string())
    };
    let name = tag("command-name")?;
    Some(match tag("command-args") {
        Some(args) if !args.is_empty() => format!("{name} {args}"),
        _ => name,
    })
}

/// Text blocks' text, images as such.
fn texts(blocks: &[Value]) -> String {
    let parts: Vec<String> = blocks
        .iter()
        .filter_map(|b| match b["type"].as_str() {
            Some("text") => b["text"].as_str().map(str::to_string),
            Some("image") => Some("[image omitted]".into()),
            _ => None,
        })
        .collect();
    parts.join("\n\n")
}

/// A tool result's content: its text, or its blocks' (images and tool references named).
fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => {
            let parts: Vec<String> = blocks
                .iter()
                .map(|b| match b["type"].as_str() {
                    Some("text") => b["text"].as_str().unwrap_or_default().to_string(),
                    Some("image") => "[image omitted]".into(),
                    Some("tool_reference") => {
                        format!("[tool {}]", b["tool_name"].as_str().unwrap_or_default())
                    }
                    _ => String::new(),
                })
                .filter(|s| !s.is_empty())
                .collect();
            parts.join("\n")
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::tests::{kinds, log};
    use serde_json::json;

    /// A transcript's lines, each given as JSON.
    fn lines(entries: &[Value]) -> Vec<Line> {
        entries
            .iter()
            .map(|e| serde_json::from_value(e.clone()).unwrap())
            .collect()
    }

    fn read(entries: &[Value]) -> Conversation {
        let lines = lines(entries);
        let mut conv = convert(&lines, "s".into());
        // the same again, with ids to compare
        let mut read = Reading {
            log: log(),
            reply: None,
            results: Vec::new(),
            names: HashMap::new(),
            starts: HashMap::new(),
            kept: None,
        };
        let chain = chain(&lines);
        for (i, l) in lines.iter().enumerate() {
            if chain.contains(&i) {
                read.line(l);
            }
        }
        read.flush();
        read.log.finish();
        conv.events = read.log.events;
        conv
    }

    fn user(uuid: &str, parent: Option<&str>, content: Value) -> Value {
        json!({ "type": "user", "uuid": uuid, "parentUuid": parent, "cwd": "/p",
                "sessionId": "cc", "timestamp": format!("2026-11-01T00:00:0{}Z", uuid.len() % 10),
                "message": { "role": "user", "content": content } })
    }

    fn reply(uuid: &str, parent: &str, id: &str, block: Value, stop: Option<&str>) -> Value {
        json!({ "type": "assistant", "uuid": uuid, "parentUuid": parent,
                "message": { "id": id, "model": "claude-opus-5-5", "role": "assistant",
                             "content": [block], "stop_reason": stop,
                             "usage": { "input_tokens": 1, "output_tokens": 2,
                                        "cache_read_input_tokens": 3,
                                        "cache_creation_input_tokens": 4 } } })
    }

    fn tool_use(id: &str) -> Value {
        json!({ "type": "tool_use", "id": id, "name": "Bash", "input": { "command": "ls" } })
    }

    fn result(id: &str, content: Value) -> Value {
        json!([{ "type": "tool_result", "tool_use_id": id, "content": content }])
    }

    #[test]
    fn a_session_reads_as_turns_with_replies_parallel_calls_and_results() {
        let conv = read(&[
            json!({ "type": "ai-title", "aiTitle": "Listing" }),
            user("u1", None, json!("list it")),
            json!({ "type": "attachment", "uuid": "a1", "parentUuid": "u1" }),
            reply(
                "r1",
                "u1",
                "m1",
                json!({ "type": "thinking", "thinking": "hm", "signature": "sig" }),
                None,
            ),
            reply("r2", "r1", "m1", tool_use("c1"), None),
            reply("r3", "r2", "m1", tool_use("c2"), Some("tool_use")),
            // the first call's result hangs off its block; the chain goes on through the second
            user("t1", Some("r2"), result("c1", json!("a\nb"))),
            user(
                "t2",
                Some("r3"),
                result(
                    "c2",
                    json!([{ "type": "text", "text": "x" }, { "type": "image" }]),
                ),
            ),
            reply(
                "r4",
                "t2",
                "m2",
                json!({ "type": "text", "text": "done" }),
                Some("end_turn"),
            ),
            json!({ "type": "system", "subtype": "turn_duration", "uuid": "d1", "parentUuid": "r4" }),
            user("u2", Some("d1"), json!("[Request interrupted by user]")),
            user(
                "u3",
                Some("u2"),
                json!("<command-name>/review</command-name><command-args>it</command-args>"),
            ),
            json!({ "type": "user", "uuid": "m", "parentUuid": "u3", "isMeta": true,
                    "message": { "role": "user", "content": "<system-reminder>x" } }),
            user(
                "u4",
                Some("m"),
                json!("<task-notification>agent done</task-notification>"),
            ),
        ]);
        assert_eq!(
            kinds(&conv.events),
            [
                "user:list it",
                "turn:e0",
                "reply:ToolUse:[thinking:hm,call:Bash:c1,call:Bash:c2]",
                "result:Bash:c1:false:a\nb",
                "result:Bash:c2:false:x\n[image omitted]",
                "reply:EndTurn:[text:done]",
                "end:Completed",
                "user:/review it",
                "turn:e7",
                "user:<task-notification>agent done</task-notification>",
                "steer:e9",
                // it ended on its user's message
                "end:Aborted",
            ]
        );
        let Event::AssistantMessage { usage, content, .. } = &conv.events[2].1 else {
            panic!()
        };
        assert_eq!(
            *usage,
            Usage {
                input: 1,
                output: 2,
                cache_read: 3,
                cache_write: 4
            }
        );
        assert!(
            matches!(&content[0], ContentBlock::Thinking { provider_data, .. } if provider_data.is_null())
        );
        assert_eq!(conv.title, Some(("Listing".into(), false)));
        assert_eq!(conv.session, "cc");
        assert_eq!(conv.cwd, PathBuf::from("/p"));
        assert_eq!(conv.model.as_deref(), Some("claude-opus-5-5"));
    }

    #[test]
    fn a_compaction_keeps_its_preserved_messages_and_an_abandoned_branch_is_left_out() {
        let conv = read(&[
            user("u1", None, json!("a")),
            reply(
                "r1",
                "u1",
                "m1",
                json!({ "type": "text", "text": "A" }),
                Some("end_turn"),
            ),
            // rewound: answered again from u1
            reply(
                "x1",
                "u1",
                "mx",
                json!({ "type": "text", "text": "lost" }),
                Some("end_turn"),
            ),
            user("u2", Some("r1"), json!("b")),
            reply(
                "r2",
                "u2",
                "m2",
                json!({ "type": "text", "text": "B" }),
                Some("end_turn"),
            ),
            json!({ "type": "system", "subtype": "compact_boundary", "uuid": "cb",
                    "parentUuid": null, "logicalParentUuid": "r2",
                    "compactMetadata": { "preservedSegment": { "headUuid": "u2" } } }),
            json!({ "type": "user", "uuid": "sum", "parentUuid": "cb", "isCompactSummary": true,
                    "message": { "role": "user", "content": "SUMMARY" } }),
            user("u3", Some("sum"), json!("c")),
            json!({ "type": "custom-title", "customTitle": "Mine" }),
        ]);
        assert_eq!(
            kinds(&conv.events),
            [
                "user:a",
                "turn:e0",
                "reply:EndTurn:[text:A]",
                "end:Completed",
                "user:b",
                "turn:e4",
                "reply:EndTurn:[text:B]",
                "compacted:e4:SUMMARY",
                "end:Completed",
                "user:c",
                "turn:e9",
                "end:Aborted",
            ]
        );
        assert_eq!(conv.title, Some(("Mine".into(), true)));
    }

    #[test]
    fn a_compaction_without_preserved_messages_keeps_nothing() {
        let conv = read(&[
            user("u1", None, json!("a")),
            json!({ "type": "system", "subtype": "compact_boundary", "uuid": "cb",
                    "parentUuid": null, "logicalParentUuid": "u1" }),
            json!({ "type": "user", "uuid": "sum", "parentUuid": "cb", "isCompactSummary": true,
                    "message": { "role": "user", "content": "S" } }),
        ]);
        assert_eq!(
            kinds(&conv.events),
            ["user:a", "turn:e0", "compacted:e2:S", "end:Aborted"]
        );
    }

    #[test]
    fn a_projects_directory_is_its_path_dashed() {
        assert_eq!(
            dashed(Path::new("/root/dirigent/master")),
            "-root-dirigent-master"
        );
        assert_eq!(dashed(Path::new("/Users/a/my.app_x")), "-Users-a-my-app-x");
    }
}
