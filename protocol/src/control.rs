//! How a player holds a session: a lease from the control plane, kept for as long as the
//! player's WebSocket to it (`/player`) is open. The player asks for one session, again until it
//! has it; control answers with the lease, or why not yet. A clean close (a Close frame) gives
//! the lease back; a dropped connection keeps it for a grace period, in which the same player can
//! reconnect and resume it, with the same epoch. Each new holder gets the next epoch. With the
//! lease comes the session's spec: what the player runs, and where its workspace comes from.

use serde::{Deserialize, Serialize};

/// The subject a player is, to chlive and in the session's grants.
pub fn player_subject(player: &str) -> String {
    format!("player:{player}")
}

/// What a session runs, set when it's made (`PUT /sessions/{id}`) and fixed from then on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSpec {
    #[serde(default = "default_model")]
    pub model: String,
    /// `None`: no extended thinking.
    #[serde(default = "default_effort")]
    pub effort: Option<Effort>,
    /// Where the workspace's files come from; `None`, an empty workspace.
    #[serde(default)]
    pub source: Option<Source>,
}

fn default_model() -> String {
    "claude-sonnet-5".into()
}

fn default_effort() -> Option<Effort> {
    Some(Effort::Medium)
}

impl Default for SessionSpec {
    fn default() -> Self {
        SessionSpec {
            model: default_model(),
            effort: default_effort(),
            source: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    High,
    Max,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// A public repository, cloned at `ref` (a branch or tag; `None`, its default branch).
    Git {
        url: String,
        #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
    },
}

/// From the player, until it holds the session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToControl {
    /// Hold `session` as `player` (its id; the subject is [`player_subject`]). Reconnecting, the
    /// player resumes the lease it held, `epoch`: it gets it back or loses it, never a new one.
    Acquire {
        session: String,
        player: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        epoch: Option<i64>,
    },
}

/// From control.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToPlayer {
    /// The player holds the session, as its `epoch`th player, and runs `spec`.
    Acquired { epoch: i64, spec: SessionSpec },
    /// Another player holds it; ask again later.
    Held,
    /// No such session (yet); ask again later.
    UnknownSession,
    /// The lease the player resumes is gone (it expired): it must stop.
    Lost,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_tagged_json() {
        let acquire = ToControl::Acquire {
            session: "s".into(),
            player: "p".into(),
            epoch: None,
        };
        let wire = serde_json::to_string(&acquire).unwrap();
        assert_eq!(wire, r#"{"type":"acquire","session":"s","player":"p"}"#);
        assert_eq!(serde_json::from_str::<ToControl>(&wire).unwrap(), acquire);
        let resume = r#"{"type":"acquire","session":"s","player":"p","epoch":3}"#;
        assert!(matches!(
            serde_json::from_str(resume).unwrap(),
            ToControl::Acquire { epoch: Some(3), .. }
        ));
        for (msg, wire) in [
            (
                ToPlayer::Acquired {
                    epoch: 2,
                    spec: SessionSpec::default(),
                },
                r#"{"type":"acquired","epoch":2,"spec":{"model":"claude-sonnet-5","effort":"medium","source":null}}"#,
            ),
            (ToPlayer::Held, r#"{"type":"held"}"#),
            (ToPlayer::UnknownSession, r#"{"type":"unknown_session"}"#),
            (ToPlayer::Lost, r#"{"type":"lost"}"#),
        ] {
            assert_eq!(serde_json::to_string(&msg).unwrap(), wire);
            assert_eq!(serde_json::from_str::<ToPlayer>(wire).unwrap(), msg);
        }
    }

    #[test]
    fn a_spec_defaults_to_sonnet_at_medium_effort_in_an_empty_workspace() {
        let spec: SessionSpec = serde_json::from_str("{}").unwrap();
        assert_eq!(spec, SessionSpec::default());
        let spec: SessionSpec = serde_json::from_str(
            r#"{"model":"m","effort":null,"source":{"git":{"url":"u","ref":"main"}}}"#,
        )
        .unwrap();
        assert_eq!(spec.effort, None);
        assert_eq!(
            spec.source,
            Some(Source::Git {
                url: "u".into(),
                reference: Some("main".into())
            })
        );
        let git = Source::Git {
            url: "u".into(),
            reference: None,
        };
        assert_eq!(
            serde_json::to_string(&git).unwrap(),
            r#"{"git":{"url":"u"}}"#
        );
    }
}
