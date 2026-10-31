//! The summary call of a compaction: what it asks, and what counts as a summary.

use crate::history::CHARS_PER_TOKEN;
use crate::provider::{Completion, ContentBlock, Message, Request, Role, StopReason};

const INSTRUCTION: &str = include_str!("../prompts/compact.md");

/// How much of each tool result and call the fallback transcript keeps.
const CLIP_CHARS: usize = 2000;

/// The next call's request with the instruction appended, so it reads the cached prefix.
pub fn request(base: &Request) -> Request {
    let mut req = base.clone();
    let instruction = ContentBlock::Text {
        text: INSTRUCTION.into(),
    };
    match req.messages.last_mut() {
        Some(last) if last.role == Role::User => last.content.push(instruction),
        _ => req.messages.push(Message {
            role: Role::User,
            content: vec![instruction],
        }),
    }
    req
}

/// For when even the summary call is too long: the conversation flattened into one message,
/// with tool results and calls clipped, without tools, and its oldest messages dropped until
/// it fits in `budget` tokens.
pub fn fallback(base: &Request, budget: u64) -> Request {
    let mut parts: Vec<String> = base.messages.iter().map(flatten).collect();
    let fixed = base.system.as_deref().map_or(0, str::len) + INSTRUCTION.len();
    let size = |parts: &[String]| (fixed + parts.iter().map(String::len).sum::<usize>()) as u64;
    let mut dropped = 0;
    while parts.len() > 1 && size(&parts[dropped..]) / CHARS_PER_TOKEN > budget {
        dropped += 1;
    }
    parts.drain(..dropped);
    let mut transcript = String::from("<transcript>\n");
    if dropped > 0 {
        transcript.push_str(&format!("[{dropped} earlier messages left out]\n\n"));
    }
    transcript.push_str(&parts.join("\n\n"));
    transcript.push_str("\n</transcript>\n\n");
    transcript.push_str(INSTRUCTION);
    Request {
        model: base.model.clone(),
        system: base.system.clone(),
        messages: vec![Message::user(transcript)],
        tools: Vec::new(),
        effort: base.effort,
    }
}

fn flatten(m: &Message) -> String {
    let role = match m.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let blocks: Vec<String> = m
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(format!("[{role}] {text}")),
            ContentBlock::Thinking { .. } => None,
            ContentBlock::ToolCall { name, input, .. } => {
                Some(format!("[tool call {name}] {}", clip(&input.to_string())))
            }
            ContentBlock::ToolResult {
                content, is_error, ..
            } => Some(format!(
                "[tool {}] {}",
                if *is_error { "error" } else { "result" },
                clip(content)
            )),
        })
        .collect();
    blocks.join("\n")
}

fn clip(s: &str) -> String {
    if s.len() <= CLIP_CHARS {
        return s.to_string();
    }
    let cut = crate::tools::floor_char_boundary(s, CLIP_CHARS);
    format!("{}… [{} more characters]", &s[..cut], s.len() - cut)
}

/// The summary in a reply, or why it isn't one. It must be smaller than the `dropped` tokens
/// it replaces, or the compaction would gain nothing.
pub fn summary(c: &Completion, dropped: u64) -> Result<String, String> {
    if c.message
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolCall { .. }))
    {
        return Err("the summary call asked for tools".into());
    }
    if c.stop_reason == StopReason::MaxTokens {
        return Err("the summary was cut off".into());
    }
    let text = c.message.text().trim().to_string();
    if text.is_empty() {
        return Err("the summary came back empty".into());
    }
    let tokens = text.len() as u64 / CHARS_PER_TOKEN;
    if tokens >= dropped {
        return Err(format!(
            "the summary ({tokens} tokens) is no smaller than what it replaces ({dropped} tokens)"
        ));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Usage;
    use serde_json::json;

    fn base(messages: Vec<Message>) -> Request {
        Request {
            model: "m".into(),
            system: Some("sys".into()),
            messages,
            tools: Vec::new(),
            effort: None,
        }
    }

    fn reply(content: Vec<ContentBlock>, stop_reason: StopReason) -> Completion {
        Completion {
            message: Message::assistant(content),
            stop_reason,
            usage: Usage::default(),
        }
    }

    #[test]
    fn the_instruction_joins_the_last_user_message() {
        let req = request(&base(vec![Message::user("a")]));
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].content.len(), 2);
        assert!(req.messages[0].text().ends_with(INSTRUCTION));
        let req = request(&base(vec![
            Message::user("a"),
            Message::assistant(vec![ContentBlock::Text { text: "r".into() }]),
        ]));
        assert_eq!(req.messages.len(), 3);
        assert_eq!(req.messages[2].text(), INSTRUCTION);
    }

    #[test]
    fn the_fallback_clips_results_and_drops_the_oldest_messages_to_fit() {
        let messages = vec![
            Message::user("old".repeat(1000)),
            Message::assistant(vec![ContentBlock::ToolCall {
                id: "c1".into(),
                name: "Read".into(),
                input: json!({ "file_path": "/a" }),
            }]),
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    call_id: "c1".into(),
                    content: "z".repeat(10_000),
                    is_error: false,
                }],
            },
        ];
        let req = fallback(&base(messages.clone()), 100_000);
        let text = req.messages[0].text();
        assert!(req.tools.is_empty() && req.messages.len() == 1);
        assert!(text.contains("[user] oldold"));
        assert!(text.contains(r#"[tool call Read] {"file_path":"/a"}"#));
        assert!(text.contains("[tool result] zzz"));
        assert!(text.contains("… [8000 more characters]"));
        assert!(text.ends_with(INSTRUCTION));

        let budget = (INSTRUCTION.len() as u64 + 2500) / CHARS_PER_TOKEN;
        let text = fallback(&base(messages), budget).messages[0].text();
        assert!(text.contains("[1 earlier messages left out]"));
        assert!(!text.contains("oldold"));
    }

    #[test]
    fn a_summary_is_text_smaller_than_what_it_replaces() {
        let text = |t: &str| vec![ContentBlock::Text { text: t.into() }];
        assert_eq!(
            summary(&reply(text("  S  "), StopReason::EndTurn), 10),
            Ok("S".into())
        );
        assert!(summary(&reply(text(" "), StopReason::EndTurn), 10).is_err());
        assert!(summary(&reply(text("S"), StopReason::MaxTokens), 10).is_err());
        assert!(summary(&reply(text(&"s".repeat(40)), StopReason::EndTurn), 10).is_err());
        let call = vec![ContentBlock::ToolCall {
            id: "c".into(),
            name: "Read".into(),
            input: json!({}),
        }];
        assert!(summary(&reply(call, StopReason::ToolUse), 10).is_err());
    }
}
