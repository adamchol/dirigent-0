//! How a player holds a session: a lease from the control plane, over gRPC ([`pb`], whose
//! contract `proto/dirigent/control/v1/players.proto` describes the lease), and what a session
//! is to control's HTTP API. The spec is kept as JSON (in Postgres and the API) and goes to the
//! player as protobuf.

use serde::{Deserialize, Serialize};

/// The gRPC contract (generated): the `Players` service, its client and server.
pub mod pb {
    tonic::include_proto!("dirigent.control.v1");
}

/// The subject a player is, to chlive and in the session's grants.
pub fn player_subject(player: &str) -> String {
    format!("player:{player}")
}

/// What a session runs, set when it's made (`PUT /sessions/{id}`). Its model, effort and
/// provider ([`Settings`]) change with `PUT /sessions/{id}/model`; the rest is fixed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSpec {
    #[serde(default = "default_model")]
    pub model: String,
    /// `None`: no extended thinking (the least there is, on a model that always thinks).
    #[serde(default = "default_effort")]
    pub effort: Option<Effort>,
    /// Where the workspace's files come from; `None`, an empty workspace.
    #[serde(default)]
    pub source: Option<Source>,
    /// What the session's player plays.
    #[serde(default, skip_serializing_if = "Part::is_coder")]
    pub part: Part,
    /// Who its model calls are paid to.
    #[serde(default, skip_serializing_if = "Provider::is_anthropic")]
    pub provider: Provider,
    /// Its machine; `None`, the cluster's default one (as it is when the session starts).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Resources>,
}

/// A player's machine: its cores, and its memory and workspace disk in GiB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Resources {
    pub cpu: u32,
    pub memory: u32,
    pub disk: u32,
}

/// Who a session's model calls are paid to: Anthropic, with the player's API key; or OpenAI,
/// with its owner's ChatGPT plan (linked through `dirigent link chatgpt`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    #[default]
    Anthropic,
    Chatgpt,
}

impl Provider {
    fn is_anthropic(&self) -> bool {
        *self == Provider::Anthropic
    }
}

/// What a session's player plays: a coder, in its workspace; or the dirigent, directing other
/// sessions as the session's owner.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Part {
    #[default]
    Coder,
    Dirigent,
}

impl Part {
    fn is_coder(&self) -> bool {
        *self == Part::Coder
    }
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
            part: Part::Coder,
            provider: Provider::Anthropic,
            resources: None,
        }
    }
}

/// What of a session's spec can change while it lives: its model, how hard it thinks, and who
/// its model calls are paid to. A running player takes a change before its next model call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    pub model: String,
    /// `None`: no extended thinking.
    pub effort: Option<Effort>,
    #[serde(default)]
    pub provider: Provider,
}

impl SessionSpec {
    pub fn settings(&self) -> Settings {
        Settings {
            model: self.model.clone(),
            effort: self.effort,
            provider: self.provider,
        }
    }

    /// Why it can't run, if it can't: a dirigent has no source, and a machine has some of each.
    pub fn check(&self) -> Result<(), String> {
        if self.part == Part::Dirigent && self.source.is_some() {
            return Err("a dirigent's session has no source".into());
        }
        if let Some(r) = self.resources {
            if r.cpu == 0 || r.memory == 0 || r.disk == 0 {
                return Err(
                    "a machine has at least 1 core, 1 GiB of memory and 1 GiB of disk".into(),
                );
            }
        }
        Ok(())
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
    /// A repository, cloned at `ref` (a branch or tag; `None`, its default branch): with the
    /// session's `GITHUB_TOKEN`, a private one on GitHub too.
    Git {
        url: String,
        #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
    },
}

impl From<SessionSpec> for pb::SessionSpec {
    fn from(spec: SessionSpec) -> Self {
        pb::SessionSpec {
            model: spec.model,
            effort: spec.effort.map(|e| pb::Effort::from(e) as i32),
            source: spec.source.map(|s| match s {
                Source::Git { url, reference } => pb::session_spec::Source::Git(pb::GitSource {
                    url,
                    r#ref: reference,
                }),
            }),
            part: match spec.part {
                Part::Coder => pb::Part::Coder,
                Part::Dirigent => pb::Part::Dirigent,
            } as i32,
            provider: pb::Provider::from(spec.provider) as i32,
            resources: spec.resources.map(|r| pb::Resources {
                cpu: r.cpu,
                memory: r.memory,
                disk: r.disk,
            }),
        }
    }
}

impl TryFrom<pb::SessionSpec> for SessionSpec {
    type Error = String;

    fn try_from(spec: pb::SessionSpec) -> Result<Self, String> {
        if spec.model.is_empty() {
            return Err("a spec without a model".into());
        }
        Ok(SessionSpec {
            model: spec.model,
            effort: effort(spec.effort).map_err(|e| format!("a spec with {e}"))?,
            source: spec.source.map(|s| match s {
                pb::session_spec::Source::Git(g) => Source::Git {
                    url: g.url,
                    reference: g.r#ref,
                },
            }),
            part: match pb::Part::try_from(spec.part) {
                Ok(pb::Part::Coder | pb::Part::Unspecified) => Part::Coder,
                Ok(pb::Part::Dirigent) => Part::Dirigent,
                Err(_) => return Err(format!("a spec with part {}, not one there is", spec.part)),
            },
            provider: provider(spec.provider).map_err(|e| format!("a spec with {e}"))?,
            resources: spec.resources.map(|r| Resources {
                cpu: r.cpu,
                memory: r.memory,
                disk: r.disk,
            }),
        })
    }
}

impl From<Settings> for pb::Settings {
    fn from(s: Settings) -> Self {
        pb::Settings {
            model: s.model,
            effort: s.effort.map(|e| pb::Effort::from(e) as i32),
            provider: pb::Provider::from(s.provider) as i32,
        }
    }
}

impl TryFrom<pb::Settings> for Settings {
    type Error = String;

    fn try_from(s: pb::Settings) -> Result<Self, String> {
        if s.model.is_empty() {
            return Err("settings without a model".into());
        }
        Ok(Settings {
            model: s.model,
            effort: effort(s.effort)?,
            provider: provider(s.provider)?,
        })
    }
}

fn effort(e: Option<i32>) -> Result<Option<Effort>, String> {
    let Some(e) = e else { return Ok(None) };
    Ok(Some(match pb::Effort::try_from(e) {
        Ok(pb::Effort::Low) => Effort::Low,
        Ok(pb::Effort::Medium) => Effort::Medium,
        Ok(pb::Effort::High) => Effort::High,
        Ok(pb::Effort::Max) => Effort::Max,
        Ok(pb::Effort::Unspecified) | Err(_) => {
            return Err(format!("effort {e}, not one there is"))
        }
    }))
}

fn provider(p: i32) -> Result<Provider, String> {
    match pb::Provider::try_from(p) {
        Ok(pb::Provider::Anthropic | pb::Provider::Unspecified) => Ok(Provider::Anthropic),
        Ok(pb::Provider::Chatgpt) => Ok(Provider::Chatgpt),
        Err(_) => Err(format!("provider {p}, not one there is")),
    }
}

impl From<Provider> for pb::Provider {
    fn from(p: Provider) -> Self {
        match p {
            Provider::Anthropic => pb::Provider::Anthropic,
            Provider::Chatgpt => pb::Provider::Chatgpt,
        }
    }
}

impl From<Effort> for pb::Effort {
    fn from(e: Effort) -> Self {
        match e {
            Effort::Low => pb::Effort::Low,
            Effort::Medium => pb::Effort::Medium,
            Effort::High => pb::Effort::High,
            Effort::Max => pb::Effort::Max,
        }
    }
}

/// A session, as control's HTTP API shows it to a caller (`GET /sessions/{id}`, `GET /sessions`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    pub spec: SessionSpec,
    /// Its holders so far; the current one's, while there is one.
    pub epoch: i64,
    /// The player holding its lease: none, the session is idle.
    pub player: Option<Holder>,
    /// Who made it: the user who writes its inputs, and starts and stops it.
    pub owner: String,
    /// Who else may read it.
    #[serde(default)]
    pub viewers: Vec<String>,
    /// The caller's.
    pub role: Role,
    /// When it was made, in milliseconds since the Unix epoch.
    #[serde(default)]
    pub created: i64,
    /// The names of the secrets its tools get (its own, and its owner's for all their
    /// sessions); never their values.
    #[serde(default)]
    pub secrets: Vec<String>,
    /// Since when it's archived (its owner put it away: it isn't started until unarchived), in
    /// milliseconds since the Unix epoch.
    #[serde(default)]
    pub archived: Option<i64>,
    /// Where its owner placed it: one of their hosts, by name; none, the cluster.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
}

/// A user's part in a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Owner,
    Viewer,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    pub subject: String,
    /// Connected to control; `false` while it's dropped and may come back for it.
    pub attached: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_shows_its_holder() {
        let info = SessionInfo {
            id: "s".into(),
            spec: SessionSpec::default(),
            epoch: 3,
            player: Some(Holder {
                subject: "player:p".into(),
                attached: false,
            }),
            owner: "user:o".into(),
            viewers: vec!["user:v".into()],
            role: Role::Viewer,
            created: 0,
            secrets: vec!["GITHUB_TOKEN".into()],
            archived: None,
            environment: None,
        };
        let wire = serde_json::to_value(&info).unwrap();
        assert_eq!(
            wire["player"],
            serde_json::json!({"subject": "player:p", "attached": false})
        );
        assert_eq!(serde_json::from_value::<SessionInfo>(wire).unwrap(), info);
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
        // Anthropic's goes unsaid, as specs made before there was a choice say it
        let chatgpt: SessionSpec = serde_json::from_str(r#"{"provider":"chatgpt"}"#).unwrap();
        assert_eq!(chatgpt.provider, Provider::Chatgpt);
        let default = serde_json::to_value(SessionSpec::default()).unwrap();
        assert!(default.get("provider").is_none());
        // nor the default machine
        assert!(default.get("resources").is_none());
    }

    #[test]
    fn a_spec_goes_through_protobuf_as_it_is() {
        use prost::Message;
        let specs = [
            SessionSpec::default(),
            SessionSpec {
                model: "m".into(),
                effort: None,
                source: Some(Source::Git {
                    url: "u".into(),
                    reference: Some("main".into()),
                }),
                part: Part::Coder,
                provider: Provider::Anthropic,
                resources: None,
            },
            SessionSpec {
                model: "m".into(),
                effort: Some(Effort::Max),
                source: Some(Source::Git {
                    url: "u".into(),
                    reference: None,
                }),
                part: Part::Coder,
                provider: Provider::Chatgpt,
                resources: Some(Resources {
                    cpu: 4,
                    memory: 8,
                    disk: 30,
                }),
            },
            SessionSpec {
                model: "m".into(),
                effort: None,
                source: None,
                part: Part::Dirigent,
                provider: Provider::Anthropic,
                resources: None,
            },
        ];
        for spec in specs {
            let wire = pb::SessionSpec::from(spec.clone()).encode_to_vec();
            let back = pb::SessionSpec::decode(wire.as_slice()).unwrap();
            assert_eq!(SessionSpec::try_from(back).unwrap(), spec);
        }
        let unspecified = pb::SessionSpec {
            model: "m".into(),
            effort: Some(pb::Effort::Unspecified as i32),
            source: None,
            part: 0,
            provider: 0,
            resources: None,
        };
        assert!(SessionSpec::try_from(unspecified).is_err());
        assert!(SessionSpec::try_from(pb::SessionSpec::default()).is_err());
    }
}
