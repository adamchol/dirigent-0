//! The call that names a session: what it asks, and what counts as a title and description.

use serde::Deserialize;

use crate::agent::AgentConfig;
use crate::provider::{Completion, ContentBlock, Message, Request};

pub const INSTRUCTION: &str = include_str!("../prompts/title.md");

/// How much of the session's text the call reads.
const INPUT_CHARS: usize = 8000;
const TITLE_CHARS: usize = 80;
const DESCRIPTION_CHARS: usize = 300;

/// The call naming a session about `input` (its first message, or a compaction's summary),
/// which `current` names now, if anything does.
pub fn request(cfg: &AgentConfig, input: &str, current: Option<&(String, String)>) -> Request {
    let mut text = String::new();
    if let Some((title, description)) = current {
        text.push_str(&format!(
            "<current>\nTitle: {title}\nDescription: {description}\n</current>\n\n"
        ));
    }
    text.push_str(&format!(
        "<session>\n{}\n</session>",
        clip(input, INPUT_CHARS)
    ));
    Request {
        model: cfg.title_model.to_string(),
        system: Some(INSTRUCTION.into()),
        messages: vec![Message::user(text)],
        tools: Vec::new(),
        effort: cfg.title_effort,
    }
}

/// The title and description in a reply: its JSON object's, each made one line and clipped;
/// `None` if it has none, or either is empty.
pub fn parse(c: &Completion) -> Option<(String, String)> {
    #[derive(Deserialize)]
    struct Named {
        title: String,
        description: String,
    }
    if c.message
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolCall { .. }))
    {
        return None;
    }
    let text = c.message.text();
    let json = text.get(text.find('{')?..=text.rfind('}')?)?;
    let named: Named = serde_json::from_str(json).ok()?;
    let line = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = line(&named.title);
    let title = title.trim_matches(|c: char| c == '"' || c == '\'' || c == '.');
    let (title, description) = (
        clip(title, TITLE_CHARS),
        clip(&line(&named.description), DESCRIPTION_CHARS),
    );
    (!title.is_empty() && !description.is_empty()).then_some((title, description))
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    format!("{}…", &s[..crate::tools::floor_char_boundary(s, max)])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Role, StopReason, Usage};

    fn reply(text: &str) -> Completion {
        Completion {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text { text: text.into() }],
            },
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        }
    }

    #[test]
    fn a_title_is_the_replys_json_made_one_line_and_clipped() {
        assert_eq!(
            parse(&reply(
                "Sure:\n```json\n{\"title\": \"\\\"Fix the\\nlogin test.\\\"\", \"description\": \"  Makes it   pass.\"}\n```"
            )),
            Some(("Fix the login test".into(), "Makes it pass.".into()))
        );
        let long = "word ".repeat(100);
        let (title, description) = parse(&reply(&format!(
            "{{\"title\": \"{long}\", \"description\": \"{long}\"}}"
        )))
        .unwrap();
        assert!(title.len() <= TITLE_CHARS + '…'.len_utf8() && title.ends_with('…'));
        assert!(description.len() <= DESCRIPTION_CHARS + '…'.len_utf8());
    }

    #[test]
    fn a_reply_without_both_is_no_title() {
        assert_eq!(parse(&reply("Fix the login test")), None);
        assert_eq!(parse(&reply("{\"title\": \"x\"}")), None);
        assert_eq!(
            parse(&reply("{\"title\": \" \", \"description\": \"d\"}")),
            None
        );
        assert_eq!(parse(&reply("} {")), None);
    }

    #[test]
    fn a_retitle_reads_the_current_name_and_the_summary() {
        let cfg = AgentConfig {
            model: "m",
            system: "s",
            effort: None,
            context_window: 1000,
            title_model: "small",
            title_effort: None,
        };
        let current = ("Old".to_string(), "Was doing this.".to_string());
        let req = request(&cfg, "the summary", Some(&current));
        assert_eq!(req.model, "small");
        assert_eq!(req.system.as_deref(), Some(INSTRUCTION));
        assert!(req.tools.is_empty());
        assert_eq!(
            req.messages[0].text(),
            "<current>\nTitle: Old\nDescription: Was doing this.\n</current>\n\n<session>\nthe summary\n</session>"
        );
    }
}
