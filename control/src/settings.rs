//! Each user's settings: which there are, and their defaults. They're kept as JSON (Postgres'
//! `user_settings`), read back through [`Settings`], so a setting no longer known, or one
//! missing, falls back to its default; what's given is checked against it, so none unknown is
//! kept.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// How much of a session's log the web's chat shows.
    pub events: Events,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Events {
    /// Messages, replies, tools, turns and errors.
    Conversation,
    /// The player's own doings too: attaching, the workspace, checkpoints, titles.
    #[default]
    Activity,
    /// Every event, each with its JSON.
    All,
}

impl Settings {
    /// What's kept, as settings: what's unknown or unreadable in it is left out.
    pub fn kept(value: serde_json::Value) -> Settings {
        let serde_json::Value::Object(fields) = value else {
            return Settings::default();
        };
        let mut settings = serde_json::Map::new();
        for (name, field) in fields {
            let one = serde_json::json!({ name.clone(): field });
            if serde_json::from_value::<Settings>(one).is_ok() {
                settings.insert(name, field);
            }
        }
        serde_json::from_value(serde_json::Value::Object(settings)).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_and_refuse_what_they_dont_know() {
        let empty: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(empty, Settings::default());
        assert_eq!(empty.events, Events::Activity);
        let all: Settings = serde_json::from_str(r#"{"events": "all"}"#).unwrap();
        assert_eq!(all.events, Events::All);
        assert!(serde_json::from_str::<Settings>(r#"{"events": "some"}"#).is_err());
        assert!(serde_json::from_str::<Settings>(r#"{"theme": "dark"}"#).is_err());
        assert_eq!(
            serde_json::to_value(&all).unwrap(),
            serde_json::json!({ "events": "all" })
        );
    }

    #[test]
    fn what_was_kept_reads_back_without_what_isnt_known_any_more() {
        let kept = serde_json::json!({ "events": "conversation", "gone": 1 });
        assert_eq!(Settings::kept(kept).events, Events::Conversation);
        let bad = serde_json::json!({ "events": "renamed" });
        assert_eq!(Settings::kept(bad), Settings::default());
        assert_eq!(Settings::kept(serde_json::json!(null)), Settings::default());
    }
}
