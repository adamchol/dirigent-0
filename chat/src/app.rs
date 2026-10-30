use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use dirigent_protocol::rows::{DeltaRow, EventRow};
use dirigent_protocol::{ContentBlock, Delta, Event, TurnEndReason, Usage};
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::Frame;
use serde_json::Value;

const RESULT_PREVIEW_LINES: usize = 4;
const THINKING_PREVIEW_LINES: usize = 3;

#[derive(Debug, PartialEq)]
pub enum Action {
    Send(String),
    Interrupt,
    Quit,
}

pub struct App {
    session: String,
    url: String,
    lines: Vec<Line<'static>>,
    /// Lines scrolled up from the bottom; 0 follows new output.
    scroll_up: usize,
    input: String,
    /// Cursor position in chars.
    cursor: usize,
    working: bool,
    turn_usage: Usage,
    /// The seq of the last event that ended a model call; deltas sent before it are done.
    call_ended: Option<u64>,
    /// Replies being streamed, by the `after_seq` of their deltas, then by block index;
    /// replaced by the finished `AssistantMessage` (or dropped on cancel or error).
    live: BTreeMap<u64, BTreeMap<usize, ContentBlock>>,
    /// Messages sent that the player hasn't logged yet, by input id.
    sent: Vec<(String, String)>,
    pub status: Option<String>,
}

impl App {
    pub fn new(session: String, url: String) -> Self {
        App {
            session,
            url,
            lines: Vec::new(),
            scroll_up: 0,
            input: String::new(),
            cursor: 0,
            working: false,
            turn_usage: Usage::default(),
            call_ended: None,
            live: BTreeMap::new(),
            sent: Vec::new(),
            status: None,
        }
    }

    /// Forget the transcript, before a snapshot of the log replaces it.
    pub fn reset(&mut self) {
        self.lines.clear();
        self.working = false;
        self.turn_usage = Usage::default();
        self.call_ended = None;
    }

    pub fn on_row(&mut self, row: &EventRow) {
        match row.event() {
            Ok(ev) => self.on_event(row.seq, ev),
            Err(e) => self.push(Line::from(format!("unreadable event {}: {e}", row.seq)).red()),
        }
    }

    pub fn on_delta_row(&mut self, row: &DeltaRow) {
        if let Ok(delta) = row.delta() {
            self.on_delta(row.after_seq, delta);
        }
    }

    /// A message the user sent, shown as waiting until the player logs it.
    pub fn sent(&mut self, input_id: String, text: String) {
        self.sent.push((input_id, text));
    }

    pub fn on_event(&mut self, seq: u64, ev: Event) {
        match ev {
            Event::UserMessage { input_id, text } => {
                self.sent.retain(|(id, _)| *id != input_id);
                if !self.working {
                    self.working = true;
                    self.turn_usage = Usage::default();
                }
                for (i, line) in text.lines().enumerate() {
                    let prefix = if i == 0 { "› " } else { "  " };
                    self.push(Line::from(format!("{prefix}{line}")).bold().cyan());
                }
            }
            Event::UserInterrupt { .. } => self.push(Line::from("⏸ interrupted").magenta()),
            Event::ModelDelta { delta } => self.on_delta(seq, delta),
            Event::AssistantMessage { content, usage, .. } => {
                self.call_ended(seq);
                self.turn_usage.input += usage.input;
                self.turn_usage.output += usage.output;
                self.turn_usage.cache_read += usage.cache_read;
                self.turn_usage.cache_write += usage.cache_write;
                for block in &content {
                    self.lines.extend(block_lines(block));
                }
            }
            Event::ModelCancelled => {
                self.call_ended(seq);
                self.push(Line::from("model call cancelled").dim());
            }
            Event::Error { message, .. } => {
                self.call_ended(seq);
                self.push(Line::from(format!("error: {message}")).red());
            }
            Event::ToolResult {
                name,
                content,
                is_error,
                ..
            } => self.tool_result(&name, &content, is_error),
            Event::TurnEnded { reason } => {
                self.working = false;
                let (label, color) = match reason {
                    TurnEndReason::Completed => ("turn completed", Color::DarkGray),
                    TurnEndReason::Aborted => ("turn aborted", Color::Magenta),
                    TurnEndReason::Failed => ("turn failed", Color::Red),
                };
                let u = &self.turn_usage;
                self.push(
                    Line::from(format!(
                        "── {label} · in {} out {} cache {} ──",
                        tokens(u.input),
                        tokens(u.output),
                        tokens(u.cache_read + u.cache_write)
                    ))
                    .fg(color),
                );
                self.push(Line::default());
            }
        }
    }

    fn call_ended(&mut self, seq: u64) {
        self.call_ended = Some(self.call_ended.map_or(seq, |s| s.max(seq)));
        self.live.retain(|after, _| *after >= seq);
    }

    /// One streaming delta of the model call that follows the log's event `after_seq`.
    pub fn on_delta(&mut self, after_seq: u64, delta: Delta) {
        if self.call_ended.is_some_and(|ended| after_seq < ended) {
            return; // its call is over
        }
        let blocks = self.live.entry(after_seq).or_default();
        match delta {
            Delta::BlockStart { index, block } => {
                blocks.insert(index, block);
            }
            Delta::Text { index, text } => match blocks.get_mut(&index) {
                Some(ContentBlock::Text { text: buf }) => buf.push_str(&text),
                _ => {
                    blocks.insert(index, ContentBlock::Text { text });
                }
            },
            Delta::Thinking { index, text } => match blocks.get_mut(&index) {
                Some(ContentBlock::Thinking { text: buf, .. }) => buf.push_str(&text),
                _ => {
                    blocks.insert(
                        index,
                        ContentBlock::Thinking {
                            text,
                            provider_data: Value::Null,
                        },
                    );
                }
            },
            Delta::ToolCallArgs { index, json } => {
                if let Some(ContentBlock::ToolCall { input, .. }) = blocks.get_mut(&index) {
                    *input = serde_json::from_str(&json).unwrap_or(Value::String(json));
                }
            }
            // Signatures, block ends, usage and stop reason only matter in the final message.
            Delta::BlockData { .. }
            | Delta::BlockStop { .. }
            | Delta::Usage(_)
            | Delta::Stop(_) => {}
        }
    }

    fn tool_result(&mut self, name: &str, content: &str, is_error: bool) {
        let lines: Vec<&str> = content.lines().collect();
        let (mark, style) = if is_error {
            ("✗", Style::new().red().bold())
        } else {
            ("✓", Style::new().green().bold())
        };
        let mut head = vec![Span::styled(format!("{mark} {name}"), style)];
        if lines.len() > 1 {
            head.push(Span::styled(
                format!("  {} lines", lines.len()),
                Style::new().dim(),
            ));
        }
        self.push(Line::from(head));
        for line in lines.iter().take(RESULT_PREVIEW_LINES) {
            self.push(Line::styled(format!("    {line}"), Style::new().dim()));
        }
        if lines.len() > RESULT_PREVIEW_LINES {
            self.push(Line::styled(
                format!("    … {} more lines", lines.len() - RESULT_PREVIEW_LINES),
                Style::new().dim(),
            ));
        }
    }

    fn push(&mut self, line: Line<'static>) {
        self.lines.push(line);
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Option<Action> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('c') if ctrl => return Some(Action::Quit),
            KeyCode::Esc => return Some(Action::Interrupt),
            KeyCode::Enter => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return None;
                }
                self.input.clear();
                self.cursor = 0;
                self.scroll_up = 0;
                return Some(Action::Send(text));
            }
            KeyCode::Char(c) if !ctrl => {
                let at = self.byte_index();
                self.input.insert(at, c);
                self.cursor += 1;
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                let at = self.byte_index();
                self.input.remove(at);
            }
            KeyCode::Delete if self.cursor < self.input.chars().count() => {
                let at = self.byte_index();
                self.input.remove(at);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.chars().count()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.chars().count(),
            KeyCode::Up => self.scroll_up += 1,
            KeyCode::Down => self.scroll_up = self.scroll_up.saturating_sub(1),
            KeyCode::PageUp => self.scroll_up += 10,
            KeyCode::PageDown => self.scroll_up = self.scroll_up.saturating_sub(10),
            _ => {}
        }
        None
    }

    fn byte_index(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.cursor)
            .map(|(i, _)| i)
            .unwrap_or(self.input.len())
    }

    /// The transcript, then the replies being streamed, then the messages not logged yet.
    fn screen(&self) -> Vec<Line<'static>> {
        let mut lines = self.lines.clone();
        for blocks in self.live.values() {
            for block in blocks.values() {
                lines.extend(block_lines(block));
            }
        }
        for (_, text) in &self.sent {
            for (i, line) in text.lines().enumerate() {
                let prefix = if i == 0 { "› " } else { "  " };
                lines.push(Line::from(format!("{prefix}{line}")).cyan().dim());
            }
        }
        lines
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let [body, status, input] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .areas(frame.area());

        let block = Block::bordered()
            .title(format!(" dirigent · {} ", self.session))
            .title_top(Line::from(format!(" {} ", self.url)).right_aligned());
        let inner = block.inner(body);
        let transcript = Paragraph::new(self.screen()).wrap(Wrap { trim: false });
        let total = transcript.line_count(inner.width);
        let max_scroll = total.saturating_sub(inner.height as usize);
        self.scroll_up = self.scroll_up.min(max_scroll);
        let offset = (max_scroll - self.scroll_up) as u16;
        frame.render_widget(transcript.block(block).scroll((offset, 0)), body);

        let status_line = match &self.status {
            Some(s) => Line::from(s.clone()).red(),
            None if self.working => {
                Line::from("working… Enter steers · Esc interrupts · Ctrl-C quits").yellow()
            }
            None if !self.sent.is_empty() => {
                Line::from("sent; waiting for the player · Esc interrupts · Ctrl-C quits").yellow()
            }
            None => Line::from("idle · Enter sends · ↑/↓ PgUp/PgDn scroll · Ctrl-C quits").dim(),
        };
        let status_line = if self.scroll_up > 0 {
            let mut l = status_line;
            l.push_span(Span::from(format!("  [scrolled up {}]", self.scroll_up)).dim());
            l
        } else {
            status_line
        };
        frame.render_widget(Paragraph::new(status_line), status);

        let input_block = Block::bordered();
        let input_inner = input_block.inner(input);
        frame.render_widget(
            Paragraph::new(format!("> {}", self.input)).block(input_block),
            input,
        );
        frame.set_cursor_position(Position::new(
            input_inner.x + 2 + self.cursor as u16,
            input_inner.y,
        ));
    }
}

/// Lines for one assistant content block (final or still streaming).
fn block_lines(block: &ContentBlock) -> Vec<Line<'static>> {
    match block {
        ContentBlock::Text { text } => text.lines().map(|l| Line::from(l.to_string())).collect(),
        ContentBlock::Thinking { text, .. } => {
            let style = Style::new().dim().italic();
            let lines: Vec<&str> = text.lines().collect();
            let mut out: Vec<Line<'static>> = lines
                .iter()
                .take(THINKING_PREVIEW_LINES)
                .map(|l| Line::styled(format!("∴ {l}"), style))
                .collect();
            if lines.len() > THINKING_PREVIEW_LINES {
                out.push(Line::styled("∴ …", style));
            }
            out
        }
        ContentBlock::ToolCall { name, input, .. } => vec![Line::from(vec![
            Span::styled(format!("▶ {name}  "), Style::new().yellow().bold()),
            Span::styled(summarize(input), Style::new().yellow()),
        ])],
        ContentBlock::ToolResult { .. } => Vec::new(),
    }
}

/// One-line summary of a tool input (or of its JSON so far, while it streams).
fn summarize(input: &Value) -> String {
    let s = ["command", "file_path", "pattern"]
        .iter()
        .find_map(|k| input.get(k).and_then(Value::as_str))
        .map(|s| s.lines().next().unwrap_or("").to_string())
        .unwrap_or_else(|| match input {
            Value::String(partial) => partial.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        });
    if s.chars().count() > 100 {
        format!("{}…", s.chars().take(100).collect::<String>())
    } else {
        s
    }
}

fn tokens(n: u64) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dirigent_protocol::StopReason;
    use serde_json::json;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn app() -> App {
        App::new("s".into(), "url".into())
    }

    fn transcript(app: &App) -> Vec<String> {
        app.lines.iter().map(|l| l.to_string()).collect()
    }

    fn screen(app: &App) -> Vec<String> {
        app.screen().iter().map(|l| l.to_string()).collect()
    }

    fn user(text: &str) -> Event {
        Event::UserMessage {
            input_id: format!("in-{text}"),
            text: text.into(),
        }
    }

    fn reply(content: Vec<ContentBlock>, usage: Usage) -> Event {
        Event::AssistantMessage {
            content,
            stop_reason: StopReason::EndTurn,
            usage,
        }
    }

    fn text(t: &str) -> ContentBlock {
        ContentBlock::Text { text: t.into() }
    }

    #[test]
    fn renders_a_turn_with_tools() {
        let mut app = app();
        app.on_event(0, user("fix it"));
        assert!(app.working);
        app.on_event(
            1,
            reply(
                vec![
                    ContentBlock::Thinking {
                        text: "a\nb\nc\nd".into(),
                        provider_data: Value::Null,
                    },
                    text("Looking."),
                    ContentBlock::ToolCall {
                        id: "c1".into(),
                        name: "Bash".into(),
                        input: json!({"command": "cargo test\n--quiet"}),
                    },
                ],
                Usage {
                    input: 1500,
                    output: 20,
                    cache_read: 0,
                    cache_write: 0,
                },
            ),
        );
        app.on_event(
            2,
            Event::ToolResult {
                call_id: "c1".into(),
                name: "Bash".into(),
                content: "1\n2\n3\n4\n5\n6".into(),
                is_error: true,
                meta: Value::Null,
            },
        );
        app.on_event(
            3,
            Event::UserInterrupt {
                input_id: "i".into(),
            },
        );
        app.on_event(4, Event::ModelCancelled);
        app.on_event(
            5,
            Event::Error {
                message: "boom".into(),
                retryable: false,
            },
        );
        app.on_event(
            6,
            Event::TurnEnded {
                reason: TurnEndReason::Aborted,
            },
        );
        assert!(!app.working);
        assert_eq!(
            transcript(&app),
            vec![
                "› fix it",
                "∴ a",
                "∴ b",
                "∴ c",
                "∴ …",
                "Looking.",
                "▶ Bash  cargo test",
                "✗ Bash  6 lines",
                "    1",
                "    2",
                "    3",
                "    4",
                "    … 2 more lines",
                "⏸ interrupted",
                "model call cancelled",
                "error: boom",
                "── turn aborted · in 1.5k out 20 cache 0 ──",
                "",
            ]
        );
    }

    #[test]
    fn deltas_stream_live_and_the_finished_message_replaces_them() {
        let mut app = app();
        app.on_event(0, user("go"));
        app.on_delta(
            0,
            Delta::BlockStart {
                index: 0,
                block: text(""),
            },
        );
        app.on_delta(
            0,
            Delta::Text {
                index: 0,
                text: "Hel".into(),
            },
        );
        assert_eq!(screen(&app), vec!["› go", "Hel"]);
        app.on_delta(
            0,
            Delta::Text {
                index: 0,
                text: "lo".into(),
            },
        );
        app.on_delta(
            0,
            Delta::BlockStart {
                index: 1,
                block: ContentBlock::ToolCall {
                    id: "c1".into(),
                    name: "Read".into(),
                    input: Value::Null,
                },
            },
        );
        app.on_delta(
            0,
            Delta::ToolCallArgs {
                index: 1,
                json: r#"{"file_path":"/a.rs"}"#.into(),
            },
        );
        assert_eq!(screen(&app), vec!["› go", "Hello", "▶ Read  /a.rs"]);

        app.on_event(
            1,
            reply(
                vec![
                    text("Hello"),
                    ContentBlock::ToolCall {
                        id: "c1".into(),
                        name: "Read".into(),
                        input: json!({"file_path": "/a.rs"}),
                    },
                ],
                Usage::default(),
            ),
        );
        assert!(app.live.is_empty());
        assert_eq!(screen(&app), vec!["› go", "Hello", "▶ Read  /a.rs"]);

        // a late delta of the finished call is ignored
        app.on_delta(
            0,
            Delta::Text {
                index: 0,
                text: "late".into(),
            },
        );
        assert!(app.live.is_empty());

        // a cancelled call drops what it streamed
        app.on_delta(
            1,
            Delta::Text {
                index: 0,
                text: "partial".into(),
            },
        );
        app.on_event(2, Event::ModelCancelled);
        assert_eq!(
            screen(&app),
            vec!["› go", "Hello", "▶ Read  /a.rs", "model call cancelled"]
        );
    }

    #[test]
    fn deltas_of_the_next_call_survive_the_previous_reply_arriving_late() {
        let mut app = app();
        app.on_event(0, user("go"));
        // the next call's deltas (after the reply at seq 1 and its tool result at 2) arrive
        // before the log has the reply
        app.on_delta(
            2,
            Delta::Text {
                index: 0,
                text: "next".into(),
            },
        );
        app.on_event(1, reply(vec![text("first")], Usage::default()));
        assert_eq!(screen(&app), vec!["› go", "first", "next"]);
    }

    #[test]
    fn sent_messages_wait_for_the_log() {
        let mut app = app();
        app.sent("in-hi".into(), "hi".into());
        assert_eq!(screen(&app), vec!["› hi"]);
        app.on_event(0, user("hi"));
        assert!(app.sent.is_empty());
        assert_eq!(screen(&app), vec!["› hi"]);
    }

    #[test]
    fn a_new_snapshot_replaces_the_transcript() {
        let mut app = app();
        app.on_event(0, user("a"));
        app.reset();
        app.on_event(0, user("a"));
        assert_eq!(transcript(&app), vec!["› a"]);
    }

    #[test]
    fn keys_edit_input_and_map_to_actions() {
        let mut app = app();
        for c in "hllo".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Home));
        app.on_key(key(KeyCode::Right));
        app.on_key(key(KeyCode::Char('e')));
        app.on_key(key(KeyCode::End));
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "hell");
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Some(Action::Send("hell".into()))
        );
        assert!(app.input.is_empty());
        assert_eq!(app.on_key(key(KeyCode::Enter)), None);
        assert_eq!(app.on_key(key(KeyCode::Esc)), Some(Action::Interrupt));
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(Action::Quit)
        );
    }
}
