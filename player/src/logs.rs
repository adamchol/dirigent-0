//! The logs older players wrote, which this one must go on loading: `player/logs/<version>/`,
//! each a session's rows (but its deltas) as ClickHouse gives them; the dev shell's `save-log`
//! keeps one. A release adds those its own player wrote.

use std::path::Path;

use dirigent_protocol::rows::EventRow;
use dirigent_protocol::Event;

use crate::agent::{close_unfinished, replay};
use crate::history::History;

#[test]
fn old_logs_load() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("logs");
    let mut logs = 0;
    for version in std::fs::read_dir(&root).unwrap() {
        for file in std::fs::read_dir(version.unwrap().path()).unwrap() {
            load(&file.unwrap().path());
            logs += 1;
        }
    }
    assert!(logs > 0, "no logs in {}", root.display());
}

/// What the player does with the log at `path` before its first turn: every row is an event,
/// the fold and the history take them, the turn left open is closed, and the workspace's
/// commits come whole.
fn load(path: &Path) {
    let text = std::fs::read_to_string(path).unwrap();
    let (mut log, mut files) = (Vec::new(), Vec::new());
    for (n, line) in text.lines().enumerate() {
        let at = || format!("{}:{}", path.display(), n + 1);
        let row: EventRow = serde_json::from_str(line).unwrap_or_else(|e| panic!("{}: {e}", at()));
        let ev = row.event().unwrap_or_else(|e| panic!("{}: {e}", at()));
        match ev {
            Event::FilesChanged { .. } => files.push(ev),
            _ => log.push((row.id, ev)),
        }
    }
    let (handled, fold) = replay(&log);
    let history = History::from_events(&handled);
    let asked = handled
        .iter()
        .any(|(_, ev)| matches!(ev, Event::UserMessage { .. }));
    assert!(
        !asked || !history.messages().is_empty(),
        "{}: the history has no messages",
        path.display()
    );
    close_unfinished(&handled, fold.open().and_then(|t| t.interrupt.clone()));
    let mut bundles = dirigent_sync::Bundles::default();
    let whole = files.iter().filter_map(|e| bundles.push(e)).count();
    assert!(
        files.is_empty() || whole > 0,
        "{}: its files_changed make no commit",
        path.display()
    );
}
