//! Who calls control's HTTP API. Every mode takes control's own tokens (`Authorization: Bearer`,
//! for `control`: a dirigent acting as its owner). Besides: in `declared` mode (dev), whoever the
//! `dirigent-user` header says, unverified; in `oidc` mode, the subject of an OpenID provider's
//! ID token (Google's), verified with the provider's public keys under the same rules as
//! chlive's issuer for it (nix/chlive.toml): its `iss`, an audience, the required claims, and
//! the subject from a claim after a prefix.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use axum::http::HeaderMap;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use serde_json::{Map, Value};
use tracing::warn;

use crate::token::{Signer, CONTROL};

/// The header naming who calls, in `declared` auth.
pub const CALLER: &str = "dirigent-user";

/// How often the provider's keys are read again; and, for a token with a key id they don't
/// have, at most how often.
const JWKS_REFRESH: Duration = Duration::from_secs(300);
const JWKS_COOLDOWN: Duration = Duration::from_secs(30);
const LEEWAY: u64 = 5;

pub struct Auth {
    pub signer: Signer,
    /// None: `declared`.
    pub oidc: Option<Arc<Oidc>>,
}

impl Auth {
    /// The caller's subject, or why there's none.
    pub async fn caller(&self, headers: &HeaderMap) -> Result<String, String> {
        let bearer = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        if let Some(token) = bearer {
            // the issuer it claims, unverified: only to choose who verifies it
            let claimed: Map<String, Value> =
                jsonwebtoken::dangerous::insecure_decode_claims(token)
                    .map_err(|_| "a malformed token".to_string())?;
            let iss = claimed.get("iss").and_then(Value::as_str);
            if iss == Some(self.signer.issuer.as_str()) {
                return match self.signer.verify(token, CONTROL) {
                    Ok(claims) => Ok(claims.sub),
                    Err(e) => Err(format!("control's token isn't taken: {e}")),
                };
            }
            return match &self.oidc {
                Some(oidc) => oidc.verify(token).await,
                None => Err("not a token of control's".into()),
            };
        }
        if self.oidc.is_some() {
            return Err("no bearer token".into());
        }
        match headers.get(CALLER).and_then(|v| v.to_str().ok()) {
            Some(u) if !u.is_empty() => Ok(u.to_string()),
            _ => Err(format!("no {CALLER} header")),
        }
    }
}

/// `--auth oidc`: the provider whose ID tokens humans present.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct OidcArgs {
    /// The tokens' `iss`, any of these (Google's say `https://accounts.google.com` or
    /// `accounts.google.com`).
    #[arg(
        long = "oidc-issuer",
        env = "DIRIGENT_OIDC_ISSUERS",
        value_delimiter = ','
    )]
    pub issuers: Vec<String>,
    /// What their `aud` must be one of: the apps' client ids; the first is the web's.
    #[arg(
        long = "oidc-audience",
        env = "DIRIGENT_OIDC_AUDIENCES",
        value_delimiter = ','
    )]
    pub audiences: Vec<String>,
    /// The provider's public keys (a JWKS): a URL, or a file.
    #[arg(
        long = "oidc-jwks",
        env = "DIRIGENT_OIDC_JWKS",
        default_value = "https://www.googleapis.com/oauth2/v3/certs"
    )]
    pub jwks: String,
    /// The claim holding the subject ...
    #[arg(long = "oidc-subject-claim", default_value = "email")]
    pub subject_claim: String,
    /// ... after this prefix.
    #[arg(long = "oidc-subject-prefix", default_value = "user:")]
    pub subject_prefix: String,
    /// `claim=value`: a claim that must have exactly this value (JSON, or else a string), e.g.
    /// `email_verified=true`, `hd=adamchol.com`.
    #[arg(
        long = "oidc-required-claim",
        env = "DIRIGENT_OIDC_REQUIRED_CLAIMS",
        value_delimiter = ',',
        value_parser = required_claim
    )]
    pub required_claims: Vec<(String, Value)>,
}

fn required_claim(s: &str) -> Result<(String, Value), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("{s:?} isn't claim=value"))?;
    if k.is_empty() {
        return Err(format!("{s:?} names no claim"));
    }
    let v = serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.into()));
    Ok((k.into(), v))
}

enum Source {
    Url(String),
    File(PathBuf),
}

/// Verifies an OpenID provider's ID tokens.
pub struct Oidc {
    issuers: Vec<String>,
    audiences: Vec<String>,
    subject_claim: String,
    subject_prefix: String,
    required_claims: BTreeMap<String, Value>,
    source: Source,
    keys: RwLock<JwkSet>,
    /// When the keys were last read; held while they're read again.
    loaded_at: tokio::sync::Mutex<Instant>,
    http: reqwest::Client,
}

impl Oidc {
    /// Its keys read (failing without them), then read again every [`JWKS_REFRESH`].
    pub async fn start(args: &OidcArgs) -> anyhow::Result<Arc<Oidc>> {
        anyhow::ensure!(!args.issuers.is_empty(), "--auth oidc needs --oidc-issuer");
        anyhow::ensure!(
            !args.audiences.is_empty(),
            "--auth oidc needs --oidc-audience (the apps' client ids)"
        );
        let source = if args.jwks.starts_with("https://") || args.jwks.starts_with("http://") {
            Source::Url(args.jwks.clone())
        } else {
            Source::File(args.jwks.clone().into())
        };
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()?;
        let keys = load(&http, &source).await?;
        let oidc = Arc::new(Oidc {
            issuers: args.issuers.clone(),
            audiences: args.audiences.clone(),
            subject_claim: args.subject_claim.clone(),
            subject_prefix: args.subject_prefix.clone(),
            required_claims: args.required_claims.iter().cloned().collect(),
            source,
            keys: RwLock::new(keys),
            loaded_at: tokio::sync::Mutex::new(Instant::now()),
            http,
        });
        let weak = Arc::downgrade(&oidc);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(JWKS_REFRESH).await;
                let Some(oidc) = weak.upgrade() else { return };
                let mut loaded_at = oidc.loaded_at.lock().await;
                oidc.reload().await;
                *loaded_at = Instant::now();
            }
        });
        Ok(oidc)
    }

    /// The web's client id.
    pub fn client_id(&self) -> &str {
        &self.audiences[0]
    }

    /// The token's subject, or why it isn't taken.
    pub async fn verify(&self, token: &str) -> Result<String, String> {
        let header = jsonwebtoken::decode_header(token).map_err(|_| "a malformed token")?;
        if matches!(
            header.alg,
            Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512
        ) {
            return Err("a token signed with a shared secret".into());
        }
        let key = match self.key(header.kid.as_deref()) {
            Some(key) => key,
            None => {
                // a key it doesn't have: read them again, unless that was just done
                let mut loaded_at = self.loaded_at.lock().await;
                if loaded_at.elapsed() >= JWKS_COOLDOWN {
                    self.reload().await;
                    *loaded_at = Instant::now();
                }
                drop(loaded_at);
                self.key(header.kid.as_deref())
                    .ok_or("signed with a key the provider doesn't have")?
            }
        };
        let mut validation = Validation::new(header.alg);
        validation.set_issuer(&self.issuers);
        validation.set_audience(&self.audiences);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        validation.validate_nbf = true;
        validation.leeway = LEEWAY;
        let claims = jsonwebtoken::decode::<Map<String, Value>>(token, &key, &validation)
            .map_err(|e| format!("the token isn't taken: {e}"))?
            .claims;
        for (claim, want) in &self.required_claims {
            if claims.get(claim) != Some(want) {
                return Err(format!("the token's {claim} isn't {want}"));
            }
        }
        match claims.get(&self.subject_claim).and_then(Value::as_str) {
            Some(s) if !s.is_empty() => Ok(format!("{}{s}", self.subject_prefix)),
            _ => Err(format!("the token has no {}", self.subject_claim)),
        }
    }

    /// The key named `kid` (or, with none named, the only one).
    fn key(&self, kid: Option<&str>) -> Option<DecodingKey> {
        let keys = self.keys.read().unwrap();
        let jwk = match kid {
            Some(kid) => keys.find(kid)?,
            None if keys.keys.len() == 1 => &keys.keys[0],
            None => return None,
        };
        DecodingKey::from_jwk(jwk).ok()
    }

    /// Read the keys again; failing, keep the ones it has.
    async fn reload(&self) {
        match load(&self.http, &self.source).await {
            Ok(keys) => *self.keys.write().unwrap() = keys,
            Err(e) => warn!("reading the OpenID provider's keys again: {e:#}"),
        }
    }
}

async fn load(http: &reqwest::Client, source: &Source) -> anyhow::Result<JwkSet> {
    match source {
        Source::Url(url) => Ok(http
            .get(url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .with_context(|| format!("fetching {url}"))?
            .json()
            .await
            .with_context(|| format!("{url}: not a JWKS"))?),
        Source::File(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_str(&text).with_context(|| format!("{}: not a JWKS", path.display()))
        }
    }
}

/// An OpenID provider of the tests': its key, and its public half in a JWKS file.
#[cfg(test)]
pub mod test_provider {
    use super::*;
    use crate::token::test_signer;

    pub const ISSUER: &str = "https://accounts.test";
    pub const CLIENT: &str = "web-client";

    pub struct Provider {
        pub signer: Signer,
        pub dir: tempfile::TempDir,
    }

    impl Provider {
        pub fn new() -> Provider {
            let signer = test_signer(ISSUER, Duration::from_secs(600));
            let dir = tempfile::tempdir().unwrap();
            let p = Provider { signer, dir };
            p.publish(&[&p.signer]);
            p
        }

        /// Its keys are now these.
        pub fn publish(&self, signers: &[&Signer]) {
            let keys: Vec<Value> = signers
                .iter()
                .map(|s| s.jwks()["keys"][0].clone())
                .collect();
            let jwks = serde_json::json!({ "keys": keys });
            std::fs::write(self.jwks(), jwks.to_string()).unwrap();
        }

        pub fn jwks(&self) -> PathBuf {
            self.dir.path().join("jwks.json")
        }

        /// The flags for it, Google-like: `email` after `user:`, verified.
        pub fn args(&self) -> OidcArgs {
            OidcArgs {
                issuers: vec![ISSUER.into()],
                audiences: vec![CLIENT.into(), "ios-client".into()],
                jwks: self.jwks().display().to_string(),
                subject_claim: "email".into(),
                subject_prefix: "user:".into(),
                required_claims: vec![("email_verified".into(), Value::Bool(true))],
            }
        }

        /// An ID token with these claims (`iss`, `exp` and `iat` added), signed by `signer`.
        pub fn token_by(signer: &Signer, claims: Value) -> String {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            let mut c = serde_json::json!({ "iss": ISSUER, "iat": now, "exp": now + 600 });
            c.as_object_mut()
                .unwrap()
                .extend(claims.as_object().unwrap().clone());
            signer.sign_claims(&c)
        }

        /// Google's, for `email`, verified, for the web.
        pub fn id_token(&self, email: &str) -> String {
            Provider::token_by(
                &self.signer,
                serde_json::json!({ "aud": CLIENT, "email": email, "email_verified": true }),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_provider::*;
    use super::*;
    use crate::token::test_signer;

    async fn oidc(p: &Provider) -> Arc<Oidc> {
        Oidc::start(&p.args()).await.unwrap()
    }

    #[tokio::test]
    async fn an_id_token_is_its_email_after_the_prefix() {
        let p = Provider::new();
        let o = oidc(&p).await;
        assert_eq!(o.client_id(), CLIENT);
        assert_eq!(
            o.verify(&p.id_token("me@x.com")).await.unwrap(),
            "user:me@x.com"
        );
        // another app's client id is as good
        let t = Provider::token_by(
            &p.signer,
            serde_json::json!({ "aud": "ios-client", "email": "me@x.com", "email_verified": true }),
        );
        assert_eq!(o.verify(&t).await.unwrap(), "user:me@x.com");
    }

    #[tokio::test]
    async fn a_token_for_another_app_unverified_expired_or_by_another_key_isnt_taken() {
        let p = Provider::new();
        let o = oidc(&p).await;
        let bad = [
            serde_json::json!({ "aud": "elsewhere", "email": "me@x.com", "email_verified": true }),
            serde_json::json!({ "aud": CLIENT, "email": "me@x.com", "email_verified": false }),
            serde_json::json!({ "aud": CLIENT, "email": "me@x.com" }),
            serde_json::json!({ "aud": CLIENT, "email_verified": true }),
            serde_json::json!({ "aud": CLIENT, "email": "me@x.com", "email_verified": true,
                "exp": 1000, "iat": 900 }),
            serde_json::json!({ "aud": CLIENT, "email": "me@x.com", "email_verified": true,
                "iss": "https://elsewhere" }),
        ];
        for claims in bad {
            let t = Provider::token_by(&p.signer, claims.clone());
            assert!(o.verify(&t).await.is_err(), "{claims} was taken");
        }
        let stranger = test_signer(ISSUER, Duration::from_secs(600));
        let t = Provider::token_by(
            &stranger,
            serde_json::json!({ "aud": CLIENT, "email": "me@x.com", "email_verified": true }),
        );
        assert!(o.verify(&t).await.is_err());
    }

    #[tokio::test]
    async fn a_new_key_is_read_when_a_token_names_it() {
        let p = Provider::new();
        let o = oidc(&p).await;
        let rotated = test_signer(ISSUER, Duration::from_secs(600));
        p.publish(&[&p.signer, &rotated]);
        // read just now: not again within the cooldown
        *o.loaded_at.lock().await = Instant::now() - JWKS_COOLDOWN;
        let t = Provider::token_by(
            &rotated,
            serde_json::json!({ "aud": CLIENT, "email": "me@x.com", "email_verified": true }),
        );
        assert_eq!(o.verify(&t).await.unwrap(), "user:me@x.com");
    }

    #[test]
    fn a_required_claim_is_json_or_a_string() {
        assert_eq!(
            required_claim("email_verified=true").unwrap(),
            ("email_verified".into(), Value::Bool(true))
        );
        assert_eq!(
            required_claim("hd=adamchol.com").unwrap(),
            ("hd".into(), Value::String("adamchol.com".into()))
        );
        assert!(required_claim("hd").is_err());
        assert!(required_claim("=x").is_err());
    }

    #[tokio::test]
    async fn declared_takes_the_header_and_controls_tokens_for_control() {
        let auth = Auth {
            signer: test_signer("control-test", Duration::from_secs(600)),
            oidc: None,
        };
        let mut h = HeaderMap::new();
        assert!(auth.caller(&h).await.is_err());
        h.insert(CALLER, "user:a".parse().unwrap());
        assert_eq!(auth.caller(&h).await.unwrap(), "user:a");

        let owner = auth
            .signer
            .sign("user:o", &[crate::token::CHLIVE, CONTROL], Some("player:d"));
        h.insert("authorization", format!("Bearer {owner}").parse().unwrap());
        assert_eq!(auth.caller(&h).await.unwrap(), "user:o");
        // a player's, for chlive only
        let player = auth.signer.sign("player:p", &[crate::token::CHLIVE], None);
        h.insert("authorization", format!("Bearer {player}").parse().unwrap());
        assert!(auth.caller(&h).await.is_err());
    }

    #[tokio::test]
    async fn oidc_takes_the_providers_tokens_and_not_the_header() {
        let p = Provider::new();
        let auth = Auth {
            signer: test_signer("control-test", Duration::from_secs(600)),
            oidc: Some(oidc(&p).await),
        };
        let mut h = HeaderMap::new();
        h.insert(CALLER, "user:a".parse().unwrap());
        assert!(auth.caller(&h).await.is_err());
        let t = p.id_token("me@x.com");
        h.insert("authorization", format!("Bearer {t}").parse().unwrap());
        assert_eq!(auth.caller(&h).await.unwrap(), "user:me@x.com");
    }
}
