//! What sessions run with besides their spec: the model provider's API key (the owner's, or the
//! install's), and secrets for their tools (the owner's for all their sessions, and each
//! session's own), kept sealed in Postgres and sent to the session's player on its lease, so no
//! value is ever in the log.

use std::time::Duration;

use serde::Deserialize;

use crate::seal::Seal;

/// The provider a key is for, as `api_keys` stores it.
pub const ANTHROPIC: &str = "anthropic";
/// How big a secret may be.
pub const MAX_VALUE: usize = 64 * 1024;

#[derive(clap::Args, Debug, Clone)]
pub struct KeyArgs {
    /// The install's Anthropic API key: the one sessions whose owner gave none of their own run
    /// on. Without it, those sessions can't call Anthropic's models.
    #[arg(long, env = "ANTHROPIC_API_KEY", hide_env_values = true)]
    pub anthropic_key: Option<String>,

    /// Anthropic's API, which users' keys are checked against as they're given.
    #[arg(
        long,
        env = "DIRIGENT_ANTHROPIC_API",
        default_value = "https://api.anthropic.com"
    )]
    pub anthropic_api: String,
}

pub struct Keys {
    /// What keys and secrets are sealed with; none without a link key, and then none are kept.
    pub seal: Option<Seal>,
    /// The install's Anthropic key.
    pub house: Option<String>,
    api: String,
    http: reqwest::Client,
}

impl Keys {
    pub fn new(args: &KeyArgs, seal: Option<Seal>) -> anyhow::Result<Keys> {
        Ok(Keys {
            seal,
            house: args.anthropic_key.clone().filter(|k| !k.is_empty()),
            api: args.anthropic_api.trim_end_matches('/').into(),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()?,
        })
    }

    /// Whether Anthropic takes `key`: listing its models with it. Why not, if it doesn't.
    pub async fn check_anthropic(&self, key: &str) -> Result<(), String> {
        #[derive(Deserialize)]
        struct Refusal {
            error: Detail,
        }
        #[derive(Deserialize)]
        struct Detail {
            message: String,
        }
        let res = self
            .http
            .get(format!("{}/v1/models", self.api))
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01")
            .send()
            .await
            .map_err(|e| format!("Anthropic couldn't be reached: {e}"))?;
        let status = res.status();
        if status.is_success() {
            return Ok(());
        }
        let body = res.text().await.unwrap_or_default();
        let why = serde_json::from_str::<Refusal>(&body)
            .map(|r| r.error.message)
            .unwrap_or_else(|_| body.chars().take(200).collect());
        Err(format!("Anthropic refused the key ({status}): {why}"))
    }
}

/// A key's last four characters, all of it that's shown again.
pub fn hint(key: &str) -> String {
    let chars: Vec<char> = key.chars().collect();
    chars[chars.len().saturating_sub(4)..].iter().collect()
}

/// Whether `name` may name a secret: an environment variable's name, `[A-Z_][A-Z0-9_]{0,63}`.
pub fn check_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let first = chars
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_');
    let rest = chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_');
    if first && rest && name.len() <= 64 {
        Ok(())
    } else {
        Err(format!(
            "{name:?} isn't a secret's name: capital letters, digits and _, not starting with a \
             digit, up to 64"
        ))
    }
}

/// Whether `value` may be a secret's.
pub fn check_value(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("a secret can't be empty".into());
    }
    if value.len() > MAX_VALUE {
        return Err(format!("a secret is at most {} KiB", MAX_VALUE / 1024));
    }
    Ok(())
}

/// Anthropic's models list, for the tests: a key starting `sk-good` is taken, any other refused
/// as Anthropic refuses one.
#[cfg(test)]
pub mod fake {
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::get;
    use axum::{Json, Router};

    pub struct Anthropic {
        pub url: String,
    }

    impl Anthropic {
        pub async fn start() -> Anthropic {
            async fn models(headers: HeaderMap) -> (StatusCode, Json<serde_json::Value>) {
                let key = headers
                    .get("x-api-key")
                    .and_then(|k| k.to_str().ok())
                    .unwrap_or_default();
                if key.starts_with("sk-good") {
                    (StatusCode::OK, Json(serde_json::json!({ "data": [] })))
                } else {
                    let error = serde_json::json!({
                        "type": "error",
                        "error": { "type": "authentication_error", "message": "invalid x-api-key" },
                    });
                    (StatusCode::UNAUTHORIZED, Json(error))
                }
            }
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let app = Router::new().route("/v1/models", get(models));
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            Anthropic { url }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secrets_name_is_an_environment_variables() {
        for ok in ["GITHUB_TOKEN", "_X", "A1", &"A".repeat(64)] {
            assert_eq!(check_name(ok), Ok(()), "{ok}");
        }
        for bad in ["", "1A", "github_token", "A-B", "A B", &"A".repeat(65)] {
            assert!(check_name(bad).is_err(), "{bad}");
        }
        assert!(check_value("").is_err());
        assert!(check_value(&"x".repeat(MAX_VALUE + 1)).is_err());
        assert_eq!(check_value("x"), Ok(()));
    }

    #[test]
    fn a_hint_is_the_last_four_characters() {
        assert_eq!(hint("sk-ant-api03-abcdWXYZ"), "WXYZ");
        assert_eq!(hint("ab"), "ab");
    }

    #[tokio::test]
    async fn a_key_is_checked_with_anthropic() {
        let anthropic = fake::Anthropic::start().await;
        let args = KeyArgs {
            anthropic_key: None,
            anthropic_api: anthropic.url.clone(),
        };
        let keys = Keys::new(&args, None).unwrap();
        assert_eq!(keys.check_anthropic("sk-good-1").await, Ok(()));
        let refused = keys.check_anthropic("sk-bad").await.unwrap_err();
        assert!(refused.contains("invalid x-api-key"), "{refused}");
    }
}
