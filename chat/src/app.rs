use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use dirigent_protocol::control::Source;
use dirigent_protocol::fold::Fold;
use dirigent_protocol::rows::EventRow;
use dirigent_protocol::{ContentBlock, Delta, Event, TurnEndReason, Update, Usage};
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
    /// Whether the player's own events (attaching, the workspace, checkpoints) are shown.
    events: bool,
    lines: Vec<Line<'static>>,
    /// Lines scrolled up from the bottom; 0 follows new output.
    scroll_up: usize,
    input: String,
    /// Cursor position in chars.
    cursor: usize,
    working: bool,
    turn_usage: Usage,
    /// The log read so far: the running turn, and the messages waiting for one.
    fold: Fold,
    /// The reply being streamed, by block index; replaced by the finished `AssistantMessage`
    /// (or dropped on cancel or error).
    live: BTreeMap<usize, ContentBlock>,
    pub status: Option<String>,
}

impl App {
    pub fn new(session: String, url: String, events: bool) -> Self {
        App {
            session,
            url,
            events,
            lines: Vec::new(),
            scroll_up: 0,
            input: String::new(),
            cursor: 0,
            working: false,
            turn_usage: Usage::default(),
            fold: Fold::default(),
            live: BTreeMap::new(),
            status: None,
        }
    }

    /// Forget the log, before a snapshot of it replaces it.
    pub fn reset(&mut self) {
        self.lines.clear();
        self.working = false;
        self.turn_usage = Usage::default();
        self.fold = Fold::default();
        self.live.clear();
    }

    pub fn on_row(&mut self, row: &EventRow) {
        match row.event() {
            Ok(ev) => self.on_logged(&row.id, ev),
            Err(e) => self.push(Line::from(format!("unreadable row {}: {e}", row.offset)).red()),
        }
    }

    /// The log's next event, with its id.
    pub fn on_logged(&mut self, id: &str, ev: Event) {
        if let Some(handled) = self.fold.apply(id, &ev) {
            self.on_handled(handled);
        }
    }

    /// The running turn's id, which an interrupt names.
    pub fn turn(&self) -> Option<String> {
        self.fold.open().map(|t| t.id.clone())
    }

    fn on_handled(&mut self, ev: Event) {
        match ev {
            Event::PlayerAttached { epoch } => {
                self.system(format!("player attached · epoch {epoch}"));
            }
            Event::PlayerDetached { epoch } => {
                self.system(format!("player detached · epoch {epoch}"));
            }
            Event::PlayerFailed { error, .. } => {
                self.push(Line::from(format!("player failed: {error}")).red());
            }
            Event::WorkspaceReady {
                source,
                head,
                recreated,
            } => {
                let from = match (&source, &head) {
                    (Some(Source::Git { url, .. }), Some(head)) => {
                        format!("{url}@{}", short(head))
                    }
                    (Some(Source::Git { url, .. }), None) => url.clone(),
                    (None, _) => "empty".into(),
                };
                let again = if recreated { " · recreated" } else { "" };
                self.system(format!("workspace ready · {from}{again}"));
            }
            Event::WorkspaceRestored { snapshot } => {
                self.system(format!("workspace restored · {}", short(&snapshot)));
            }
            Event::Checkpoint { snapshot, .. } => {
                self.system(format!("checkpoint · {}", short(&snapshot)));
            }
            Event::CheckpointFailed { error, .. } => {
                self.push(Line::from(format!("checkpoint failed: {error}")).red());
            }
            Event::UserMessage { text, via } => {
                if !self.working {
                    self.working = true;
                    self.turn_usage = Usage::default();
                }
                if let Some(via) = via {
                    self.system(format!("from the dirigent {}", short(&via)));
                }
                for (i, line) in text.lines().enumerate() {
                    let prefix = if i == 0 { "› " } else { "  " };
                    self.push(Line::from(format!("{prefix}{line}")).bold().cyan());
                }
            }
            Event::SessionUpdate {
                session, update, ..
            } => {
                if !self.working {
                    self.working = true;
                    self.turn_usage = Usage::default();
                }
                let what = match update {
                    Update::TurnEnded { reason } => format!("turn {reason:?}").to_lowercase(),
                    Update::PlayerFailed { error } => format!("player failed: {error}"),
                    Update::PlayerDetached => "stopped".into(),
                };
                self.push(Line::from(format!("⇠ session {} · {what}", short(&session))).cyan());
            }
            Event::ModelDelta { delta } => self.on_delta(delta),
            Event::AssistantMessage { content, usage, .. } => {
                self.live.clear();
                self.turn_usage.input += usage.input;
                self.turn_usage.output += usage.output;
                self.turn_usage.cache_read += usage.cache_read;
                self.turn_usage.cache_write += usage.cache_write;
                for block in &content {
                    self.lines.extend(block_lines(block));
                }
            }
            Event::ModelCancelled => {
                self.live.clear();
                self.push(Line::from("model call cancelled").dim());
            }
            Event::Error {
                message,
                retry_in_ms,
                compact,
            } => {
                self.live.clear();
                self.push(match retry_in_ms {
                    Some(ms) => Line::from(format!(
                        "error: {message} · retrying in {:.1}s",
                        ms as f64 / 1000.0
                    ))
                    .yellow(),
                    None if compact => {
                        Line::from(format!("error: {message} · compacting")).yellow()
                    }
                    None => Line::from(format!("error: {message}")).red(),
                });
            }
            Event::Compacted { summary, usage, .. } => {
                self.turn_usage.input += usage.input;
                self.turn_usage.output += usage.output;
                self.turn_usage.cache_read += usage.cache_read;
                self.turn_usage.cache_write += usage.cache_write;
                self.push(
                    Line::from(format!(
                        "context compacted · summary {} tokens · kept from here",
                        tokens(summary.len() as u64 / 4)
                    ))
                    .dim(),
                );
            }
            Event::CompactionFailed { message } => {
                self.push(Line::from(format!("compaction failed: {message}")).yellow());
            }
            Event::ToolResult {
                name,
                content,
                is_error,
                ..
            } => self.tool_result(&name, &content, is_error),
            Event::TurnEnded { reason, interrupt } => {
                self.live.clear();
                self.working = false;
                if interrupt.is_some() {
                    self.push(Line::from("⏸ interrupted").magenta());
                }
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
            // the fold keeps these
            Event::UserInterrupt { .. } | Event::TurnStarted { .. } | Event::Steered { .. } => {}
        }
    }

    /// One streaming delta of the model call in flight.
    fn on_delta(&mut self, delta: Delta) {
        let blocks = &mut self.live;
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

    /// One of the player's own events, dimmed; shown only with `--events`.
    fn system(&mut self, text: String) {
        if self.events {
            self.push(Line::from(text).dim());
        }
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

    /// The transcript, then the reply being streamed, then the messages waiting for a turn.
    fn screen(&self) -> Vec<Line<'static>> {
        let mut lines = self.lines.clone();
        for block in self.live.values() {
            lines.extend(block_lines(block));
        }
        for (_, text) in self.fold.queued() {
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
            None if !self.fold.queued().is_empty() => {
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

/// An id (a commit, a snapshot), shortened for display.
fn short(id: &str) -> &str {
    &id[..id.len().min(8)]
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
        App::new("s".into(), "url".into(), true)
    }

    fn transcript(app: &App) -> Vec<String> {
        app.lines.iter().map(|l| l.to_string()).collect()
    }

    fn screen(app: &App) -> Vec<String> {
        app.screen().iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn the_players_own_events_show_only_with_the_flag_but_failures_always() {
        let events = [
            Event::PlayerAttached { epoch: 1 },
            Event::PlayerFailed {
                epoch: 1,
                error: "e".into(),
            },
        ];
        let mut quiet = App::new("s".into(), "url".into(), false);
        log(&mut quiet, events.clone());
        assert_eq!(transcript(&quiet), ["player failed: e"]);
        let mut shown = app();
        log(&mut shown, events);
        assert_eq!(
            transcript(&shown),
            ["player attached · epoch 1", "player failed: e"]
        );
    }

    /// Log the player's `events`, with ids nothing refers to.
    fn log(app: &mut App, events: impl IntoIterator<Item = Event>) {
        for ev in events {
            app.on_logged("p", ev);
        }
    }

    /// A user's message `text` (id `m-<text>`) and the turn the player starts on it (`t-<text>`).
    fn turn(app: &mut App, text: &str) {
        app.on_logged(
            &format!("m-{text}"),
            Event::UserMessage {
                text: text.into(),
                via: None,
            },
        );
        let input = format!("m-{text}");
        app.on_logged(&format!("t-{text}"), Event::TurnStarted { input });
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

    fn delta(d: Delta) -> Event {
        Event::ModelDelta { delta: d }
    }

    #[test]
    fn renders_a_turn_with_tools() {
        let mut app = app();
        turn(&mut app, "fix it");
        assert!(app.working);
        log(
            &mut app,
            [
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
                Event::ToolResult {
                    call_id: "c1".into(),
                    name: "Bash".into(),
                    content: "1\n2\n3\n4\n5\n6".into(),
                    is_error: true,
                    meta: Value::Null,
                },
                Event::UserInterrupt {
                    turn: "t-fix it".into(),
                },
                Event::ModelCancelled,
                Event::Error {
                    message: "boom".into(),
                    retry_in_ms: None,
                    compact: false,
                },
                Event::TurnEnded {
                    reason: TurnEndReason::Aborted,
                    interrupt: Some("i".into()),
                },
            ],
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
                "model call cancelled",
                "error: boom",
                "⏸ interrupted",
                "── turn aborted · in 1.5k out 20 cache 0 ──",
                "",
            ]
        );
    }

    #[test]
    fn deltas_stream_live_and_the_finished_message_replaces_them() {
        let mut app = app();
        turn(&mut app, "go");
        log(
            &mut app,
            [
                delta(Delta::BlockStart {
                    index: 0,
                    block: text(""),
                }),
                delta(Delta::Text {
                    index: 0,
                    text: "Hel".into(),
                }),
            ],
        );
        assert_eq!(screen(&app), vec!["› go", "Hel"]);
        log(
            &mut app,
            [
                delta(Delta::Text {
                    index: 0,
                    text: "lo".into(),
                }),
                delta(Delta::BlockStart {
                    index: 1,
                    block: ContentBlock::ToolCall {
                        id: "c1".into(),
                        name: "Read".into(),
                        input: Value::Null,
                    },
                }),
                delta(Delta::ToolCallArgs {
                    index: 1,
                    json: r#"{"file_path":"/a.rs"}"#.into(),
                }),
            ],
        );
        assert_eq!(screen(&app), vec!["› go", "Hello", "▶ Read  /a.rs"]);

        log(
            &mut app,
            [reply(
                vec![
                    text("Hello"),
                    ContentBlock::ToolCall {
                        id: "c1".into(),
                        name: "Read".into(),
                        input: json!({"file_path": "/a.rs"}),
                    },
                ],
                Usage::default(),
            )],
        );
        assert!(app.live.is_empty());
        assert_eq!(screen(&app), vec!["› go", "Hello", "▶ Read  /a.rs"]);

        // a cancelled call drops what it streamed
        log(
            &mut app,
            [
                delta(Delta::Text {
                    index: 0,
                    text: "partial".into(),
                }),
                Event::ModelCancelled,
            ],
        );
        assert_eq!(
            screen(&app),
            vec!["› go", "Hello", "▶ Read  /a.rs", "model call cancelled"]
        );
    }

    #[test]
    fn a_message_waits_until_a_turn_takes_it() {
        let mut app = app();
        app.on_logged(
            "m-hi",
            Event::UserMessage {
                text: "hi".into(),
                via: None,
            },
        );
        assert!(transcript(&app).is_empty());
        assert_eq!(screen(&app), vec!["› hi"]);
        app.on_logged(
            "t-hi",
            Event::TurnStarted {
                input: "m-hi".into(),
            },
        );
        assert_eq!(transcript(&app), vec!["› hi"]);
        assert_eq!(screen(&app), vec!["› hi"]);
    }

    #[test]
    fn an_interrupt_names_the_running_turn() {
        let mut app = app();
        assert_eq!(app.turn(), None);
        turn(&mut app, "go");
        assert_eq!(app.turn().as_deref(), Some("t-go"));
        log(
            &mut app,
            [Event::TurnEnded {
                reason: TurnEndReason::Completed,
                interrupt: None,
            }],
        );
        assert_eq!(app.turn(), None);
    }

    #[test]
    fn a_new_snapshot_replaces_the_transcript() {
        let mut app = app();
        turn(&mut app, "a");
        app.reset();
        turn(&mut app, "a");
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
