use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::Frame;
use serde_json::Value;

use crate::pb;
use crate::pb::event::Kind;

const RESULT_PREVIEW_LINES: usize = 4;
const THINKING_PREVIEW_LINES: usize = 3;

#[derive(Debug, PartialEq)]
pub enum Action {
    Send(String),
    Interrupt,
    Quit,
}

pub struct App {
    addr: String,
    lines: Vec<Line<'static>>,
    /// Lines scrolled up from the bottom; 0 follows new output.
    scroll_up: usize,
    input: String,
    /// Cursor position in chars.
    cursor: usize,
    working: bool,
    turn_usage: pb::Usage,
    /// The reply being streamed, by block index; replaced by the final
    /// `AssistantMessage` (or dropped on cancel/error).
    live: BTreeMap<u32, pb::ContentBlock>,
    pub status: Option<String>,
}

impl App {
    pub fn new(addr: String) -> Self {
        App {
            addr,
            lines: Vec::new(),
            scroll_up: 0,
            input: String::new(),
            cursor: 0,
            working: false,
            turn_usage: pb::Usage::default(),
            live: BTreeMap::new(),
            status: None,
        }
    }

    pub fn on_event(&mut self, ev: pb::Event) {
        let Some(kind) = ev.kind else { return };
        match kind {
            Kind::UserMessage(m) => {
                if !self.working {
                    self.working = true;
                    self.turn_usage = pb::Usage::default();
                }
                for (i, line) in m.text.lines().enumerate() {
                    let prefix = if i == 0 { "› " } else { "  " };
                    self.push(Line::from(format!("{prefix}{line}")).bold().cyan());
                }
            }
            Kind::UserInterrupt(_) => self.push(Line::from("⏸ interrupted").magenta()),
            Kind::ModelDelta(d) => self.on_delta(d),
            Kind::AssistantMessage(m) => {
                self.live.clear();
                if let Some(u) = m.usage {
                    self.turn_usage.input += u.input;
                    self.turn_usage.output += u.output;
                    self.turn_usage.cache_read += u.cache_read;
                    self.turn_usage.cache_write += u.cache_write;
                }
                for block in m.content {
                    self.assistant_block(block);
                }
            }
            Kind::ModelCancelled(_) => {
                self.live.clear();
                self.push(Line::from("model call cancelled").dim());
            }
            Kind::Error(e) => {
                self.live.clear();
                self.push(Line::from(format!("error: {}", e.message)).red());
            }
            Kind::ToolResult(r) => self.tool_result(r),
            Kind::TurnEnded(t) => {
                self.working = false;
                let (label, color) = match pb::TurnEndReason::try_from(t.reason) {
                    Ok(pb::TurnEndReason::Completed) => ("turn completed", Color::DarkGray),
                    Ok(pb::TurnEndReason::Aborted) => ("turn aborted", Color::Magenta),
                    Ok(pb::TurnEndReason::Failed) => ("turn failed", Color::Red),
                    _ => ("turn ended", Color::DarkGray),
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

    fn assistant_block(&mut self, block: pb::ContentBlock) {
        self.lines.extend(block_lines(&block));
    }

    fn on_delta(&mut self, d: pb::ModelDelta) {
        use pb::content_block::Kind as B;
        use pb::model_delta::Kind as D;
        match d.kind {
            Some(D::BlockStart(b)) => {
                if let Some(block) = b.block {
                    self.live.insert(b.index, block);
                }
            }
            Some(D::Text(t)) => match self.live.get_mut(&t.index).and_then(|b| b.kind.as_mut()) {
                Some(B::Text(text)) => text.text.push_str(&t.text),
                _ => {
                    self.live.insert(
                        t.index,
                        pb::ContentBlock {
                            kind: Some(B::Text(pb::Text { text: t.text })),
                        },
                    );
                }
            },
            Some(D::Thinking(t)) => {
                match self.live.get_mut(&t.index).and_then(|b| b.kind.as_mut()) {
                    Some(B::Thinking(th)) => th.text.push_str(&t.text),
                    _ => {
                        self.live.insert(
                            t.index,
                            pb::ContentBlock {
                                kind: Some(B::Thinking(pb::Thinking {
                                    text: t.text,
                                    provider_data_json: String::new(),
                                })),
                            },
                        );
                    }
                }
            }
            Some(D::ToolArgs(a)) => {
                if let Some(B::ToolCall(c)) =
                    self.live.get_mut(&a.index).and_then(|b| b.kind.as_mut())
                {
                    c.input_json = a.json;
                }
            }
            // Signatures, block ends, usage and stop reason only matter in
            // the final message.
            _ => {}
        }
    }

    fn tool_result(&mut self, r: pb::ToolResult) {
        let lines: Vec<&str> = r.content.lines().collect();
        let (mark, style) = if r.is_error {
            ("✗", Style::new().red().bold())
        } else {
            ("✓", Style::new().green().bold())
        };
        let mut head = vec![Span::styled(format!("{mark} {}", r.name), style)];
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

    pub fn draw(&mut self, frame: &mut Frame) {
        let [body, status, input] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(3),
        ])
        .areas(frame.area());

        let block = Block::bordered()
            .title(" dirigent ")
            .title_top(Line::from(format!(" {} ", self.addr)).right_aligned());
        let inner = block.inner(body);
        let mut lines = self.lines.clone();
        for block in self.live.values() {
            lines.extend(block_lines(block));
        }
        let transcript = Paragraph::new(lines).wrap(Wrap { trim: false });
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

    #[cfg(test)]
    fn transcript(&self) -> Vec<String> {
        self.lines.iter().map(|l| l.to_string()).collect()
    }
}

/// Lines for one assistant content block (final or still streaming).
fn block_lines(block: &pb::ContentBlock) -> Vec<Line<'static>> {
    use pb::content_block::Kind as B;
    match &block.kind {
        Some(B::Text(t)) => t.text.lines().map(|l| Line::from(l.to_string())).collect(),
        Some(B::Thinking(t)) => {
            let style = Style::new().dim().italic();
            let lines: Vec<&str> = t.text.lines().collect();
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
        Some(B::ToolCall(c)) => vec![Line::from(vec![
            Span::styled(format!("▶ {}  ", c.name), Style::new().yellow().bold()),
            Span::styled(summarize(&c.input_json), Style::new().yellow()),
        ])],
        None => Vec::new(),
    }
}

/// One-line summary of a tool input.
fn summarize(input_json: &str) -> String {
    let value: Value = serde_json::from_str(input_json).unwrap_or(Value::Null);
    let s = ["command", "file_path", "pattern"]
        .iter()
        .find_map(|k| value.get(k).and_then(Value::as_str))
        .map(|s| s.lines().next().unwrap_or("").to_string())
        .unwrap_or_else(|| input_json.to_string());
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
    use pb::content_block::Kind as B;

    fn ev(kind: Kind) -> pb::Event {
        pb::Event {
            ts_micros: 0,
            kind: Some(kind),
        }
    }

    fn block(kind: B) -> pb::ContentBlock {
        pb::ContentBlock { kind: Some(kind) }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn renders_a_turn_with_tools() {
        let mut app = App::new("addr".into());
        app.on_event(ev(Kind::UserMessage(pb::UserMessage {
            text: "fix it".into(),
        })));
        assert!(app.working);
        app.on_event(ev(Kind::AssistantMessage(pb::AssistantMessage {
            content: vec![
                block(B::Thinking(pb::Thinking {
                    text: "a\nb\nc\nd".into(),
                    provider_data_json: String::new(),
                })),
                block(B::Text(pb::Text {
                    text: "Looking.".into(),
                })),
                block(B::ToolCall(pb::ToolCall {
                    id: "c1".into(),
                    name: "Bash".into(),
                    input_json: r#"{"command":"cargo test\n--quiet"}"#.into(),
                })),
            ],
            stop_reason: "tool_use".into(),
            usage: Some(pb::Usage {
                input: 1500,
                output: 20,
                cache_read: 0,
                cache_write: 0,
            }),
        })));
        app.on_event(ev(Kind::ToolResult(pb::ToolResult {
            call_id: "c1".into(),
            name: "Bash".into(),
            content: "1\n2\n3\n4\n5\n6".into(),
            is_error: true,
            meta_json: String::new(),
        })));
        app.on_event(ev(Kind::UserInterrupt(pb::Interrupt {})));
        app.on_event(ev(Kind::ModelCancelled(pb::ModelCancelled {})));
        app.on_event(ev(Kind::Error(pb::Error {
            message: "boom".into(),
            retryable: false,
        })));
        app.on_event(ev(Kind::TurnEnded(pb::TurnEnded {
            reason: pb::TurnEndReason::Aborted as i32,
        })));
        assert!(!app.working);
        assert_eq!(
            app.transcript(),
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

    fn delta(kind: pb::model_delta::Kind) -> pb::Event {
        ev(Kind::ModelDelta(pb::ModelDelta { kind: Some(kind) }))
    }

    /// Committed transcript plus the live blocks, as drawn.
    fn screen(app: &App) -> Vec<String> {
        let mut out = app.transcript();
        for block in app.live.values() {
            out.extend(block_lines(block).iter().map(|l| l.to_string()));
        }
        out
    }

    #[test]
    fn deltas_stream_live_and_final_message_replaces_them() {
        use pb::model_delta::Kind as D;
        let mut app = App::new("addr".into());
        app.on_event(ev(Kind::UserMessage(pb::UserMessage { text: "go".into() })));
        app.on_event(delta(D::BlockStart(pb::BlockStart {
            index: 0,
            block: Some(block(B::Text(pb::Text {
                text: String::new(),
            }))),
        })));
        app.on_event(delta(D::Text(pb::TextDelta {
            index: 0,
            text: "Hel".into(),
        })));
        assert_eq!(screen(&app), vec!["› go", "Hel"]);
        app.on_event(delta(D::Text(pb::TextDelta {
            index: 0,
            text: "lo".into(),
        })));
        app.on_event(delta(D::BlockStart(pb::BlockStart {
            index: 1,
            block: Some(block(B::ToolCall(pb::ToolCall {
                id: "c1".into(),
                name: "Read".into(),
                input_json: String::new(),
            }))),
        })));
        app.on_event(delta(D::ToolArgs(pb::ToolArgs {
            index: 1,
            json: r#"{"file_path":"/a.rs"}"#.into(),
        })));
        assert_eq!(screen(&app), vec!["› go", "Hello", "▶ Read  /a.rs"]);

        app.on_event(ev(Kind::AssistantMessage(pb::AssistantMessage {
            content: vec![
                block(B::Text(pb::Text {
                    text: "Hello".into(),
                })),
                block(B::ToolCall(pb::ToolCall {
                    id: "c1".into(),
                    name: "Read".into(),
                    input_json: r#"{"file_path":"/a.rs"}"#.into(),
                })),
            ],
            stop_reason: "tool_use".into(),
            usage: None,
        })));
        assert!(app.live.is_empty());
        assert_eq!(screen(&app), vec!["› go", "Hello", "▶ Read  /a.rs"]);

        // A cancelled call drops what was streamed so far.
        app.on_event(delta(D::Text(pb::TextDelta {
            index: 0,
            text: "partial".into(),
        })));
        app.on_event(ev(Kind::ModelCancelled(pb::ModelCancelled {})));
        assert_eq!(
            screen(&app),
            vec!["› go", "Hello", "▶ Read  /a.rs", "model call cancelled"]
        );
    }

    #[test]
    fn keys_edit_input_and_map_to_actions() {
        let mut app = App::new("addr".into());
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
