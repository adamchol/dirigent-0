//! How a log reads: folding its events in order gives what was *handled*, in the order it was
//! handled. An input (a user's message, or a dirigent's session update) counts where the player
//! took it (`TurnStarted` or `Steered`), as a [`Event::UserMessage`] of its text; until then it
//! is queued. An interrupt counts only for the turn it
//! names while that turn is running; the player records the result as `TurnEnded`.

use crate::Event;

/// The running turn, as the log tells it.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenTurn {
    /// Its `TurnStarted`'s id.
    pub id: String,
    /// The first user interrupt for it, if any came.
    pub interrupt: Option<String>,
}

#[derive(Debug, Default)]
pub struct Fold {
    queued: Vec<(String, String)>,
    open: Option<OpenTurn>,
}

impl Fold {
    /// The next event of the log, with its row's `id`. Returns the handled event it amounts to:
    /// a taken message as `UserMessage`, the player's other events as they are; `None` for
    /// what isn't handled (yet).
    pub fn apply(&mut self, id: &str, ev: &Event) -> Option<Event> {
        match ev {
            Event::UserMessage { .. } | Event::SessionUpdate { .. } => {
                let text = ev.input().expect("an input");
                self.queued.push((id.to_string(), text));
                None
            }
            Event::UserInterrupt { turn } => {
                if let Some(t) = &mut self.open {
                    if t.id == *turn && t.interrupt.is_none() {
                        t.interrupt = Some(id.to_string());
                    }
                }
                None
            }
            Event::TurnStarted { input } => {
                self.open = Some(OpenTurn {
                    id: id.to_string(),
                    interrupt: None,
                });
                self.take(input)
            }
            Event::Steered { input } => self.take(input),
            Event::TurnEnded { .. } => {
                self.open = None;
                Some(ev.clone())
            }
            Event::PlayerAttached { .. }
            | Event::PlayerDetached { .. }
            | Event::WorkspaceReady { .. }
            | Event::PlayerFailed { .. }
            | Event::WorkspaceRestored { .. }
            | Event::Checkpoint { .. }
            | Event::CheckpointFailed { .. }
            | Event::ModelDelta { .. }
            | Event::AssistantMessage { .. }
            | Event::ModelCancelled
            | Event::Error { .. }
            | Event::Compacted { .. }
            | Event::CompactionFailed { .. }
            | Event::ToolResult { .. }
            | Event::Titled { .. } => Some(ev.clone()),
        }
    }

    /// Inputs no turn has taken, oldest first: `(id, text)`.
    pub fn queued(&self) -> &[(String, String)] {
        &self.queued
    }

    pub fn open(&self) -> Option<&OpenTurn> {
        self.open.as_ref()
    }

    fn take(&mut self, input: &str) -> Option<Event> {
        let pos = self.queued.iter().position(|(id, _)| id == input)?;
        let (_, text) = self.queued.remove(pos);
        Some(Event::UserMessage { text, via: None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TurnEndReason;

    fn fold(log: &[(&str, Event)]) -> (Fold, Vec<Event>) {
        let mut f = Fold::default();
        let handled = log.iter().filter_map(|(id, ev)| f.apply(id, ev)).collect();
        (f, handled)
    }

    fn msg(text: &str) -> Event {
        Event::UserMessage {
            text: text.into(),
            via: None,
        }
    }

    fn start(input: &str) -> Event {
        Event::TurnStarted {
            input: input.into(),
        }
    }

    fn ended(interrupt: Option<&str>) -> Event {
        Event::TurnEnded {
            reason: TurnEndReason::Completed,
            interrupt: interrupt.map(Into::into),
        }
    }

    #[test]
    fn a_message_counts_where_a_turn_takes_it() {
        let (f, handled) = fold(&[("m1", msg("a")), ("m2", msg("b")), ("t1", start("m2"))]);
        assert_eq!(handled, vec![msg("b")]);
        assert_eq!(f.queued(), [("m1".to_string(), "a".to_string())]);
        assert_eq!(f.open().map(|t| t.id.as_str()), Some("t1"));
    }

    #[test]
    fn a_steer_joins_the_running_turn() {
        let (f, handled) = fold(&[
            ("m1", msg("a")),
            ("t1", start("m1")),
            ("m2", msg("b")),
            ("s1", Event::Steered { input: "m2".into() }),
            ("e1", ended(None)),
        ]);
        assert_eq!(handled, vec![msg("a"), msg("b"), ended(None)]);
        assert!(f.queued().is_empty() && f.open().is_none());
    }

    #[test]
    fn only_an_interrupt_for_the_running_turn_counts() {
        let stale = Event::UserInterrupt { turn: "t0".into() };
        let current = Event::UserInterrupt { turn: "t1".into() };
        let (f, handled) = fold(&[
            ("m1", msg("a")),
            ("t1", start("m1")),
            ("i0", stale),
            ("i1", current.clone()),
            ("i2", current),
        ]);
        assert_eq!(handled, vec![msg("a")]);
        assert_eq!(
            f.open(),
            Some(&OpenTurn {
                id: "t1".into(),
                interrupt: Some("i1".into())
            })
        );
        let (f, _) = fold(&[("i1", Event::UserInterrupt { turn: "t1".into() })]);
        assert!(f.open().is_none());
    }

    #[test]
    fn the_players_events_pass_through_and_a_turn_end_closes_the_turn() {
        let (f, handled) = fold(&[
            ("m1", msg("a")),
            ("t1", start("m1")),
            ("c1", Event::ModelCancelled),
            ("e1", ended(Some("i1"))),
            ("m2", msg("b")),
        ]);
        assert_eq!(
            handled,
            vec![msg("a"), Event::ModelCancelled, ended(Some("i1"))]
        );
        assert!(f.open().is_none());
        assert_eq!(f.queued().len(), 1);
    }
}
