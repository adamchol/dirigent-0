//! ChatGPT plans linked to dirigent's users, which their sessions' model calls are paid with:
//! OpenAI's Sign in with ChatGPT, its plan usage for self-hosted tools. The user signs in in
//! their browser, through the CLI (OpenAI's redirect goes only to a loopback address, which only
//! a process on their machine has); control makes the authorization and redeems it, so the
//! grant never leaves it. Control alone renews it: every renewal replaces the refresh token (an
//! old one used again is refused), so renewals of a link are one at a time, and the players get
//! only its access tokens, an hour's, on their leases.
//!
//! The host OpenAI knows the grant's usage by is this control (it holds and renews the grant),
//! named by its signing key's thumbprint (RFC 9278), as OpenAI recommends.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use jsonwebtoken::Validation;
use rand::RngCore;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::auth::{asymmetric_header, Keys, LEEWAY};
use crate::leases::{Link, Store};

/// The scope that lets a grant's access tokens be paid with the plan.
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// What the access tokens are for.
const RESOURCE: &str = "https://api.openai.com/v1";
/// What a first sign-in registers its client with, and as.
const DYNAMIC_CLIENT: &str = "dynamic_agent_client";
const AGENT_NAME: &str = "dirigent";
/// How long a sign-in may take, from its start to its finish.
const PENDING: Duration = Duration::from_secs(600);
/// An access token with less left than this is renewed before it's handed out: a player's
/// lease gets one at least every half of control's token TTL (5 min by default).
const RENEW_BEFORE: Duration = Duration::from_secs(15 * 60);
/// OpenAI's refresh errors after which the grant won't renew again: linked anew, it will.
const TERMINAL: &[&str] = &[
    "invalid_grant",
    "invalid_refresh_token",
    "token_expired",
    "refresh_token_expired",
    "refresh_token_invalidated",
    "refresh_token_reused",
    "invalid_client",
];

#[derive(clap::Args, Debug, Clone)]
pub struct ChatGptArgs {
    /// The key ChatGPT links' refresh tokens are encrypted with in Postgres (32 bytes, base64:
    /// `control keygen` makes `link.key`). Without it, no one can link a ChatGPT plan.
    #[arg(long, env = "DIRIGENT_LINK_KEY")]
    pub link_key: Option<std::path::PathBuf>,

    /// OpenAI's OpenID provider (its `/.well-known/openid-configuration`).
    #[arg(
        long,
        env = "DIRIGENT_OPENAI_AUTH",
        default_value = "https://auth.openai.com"
    )]
    pub openai_auth: String,

    /// OpenAI's API, which a plan's models are listed by.
    #[arg(
        long,
        env = "DIRIGENT_OPENAI_API",
        default_value = "https://api.openai.com/v1"
    )]
    pub openai_api: String,
}

/// Why there's no access token for a user's plan.
#[derive(Debug, thiserror::Error)]
pub enum Unavailable {
    #[error("no ChatGPT plan is linked (dirigent link chatgpt)")]
    NotLinked,
    #[error("the ChatGPT link is broken: {0} (link it again: dirigent link chatgpt)")]
    Broken(String),
    #[error("renewing the ChatGPT link failed: {0:#}")]
    Failed(anyhow::Error),
}

pub struct ChatGpt {
    http: reqwest::Client,
    auth: String,
    api: String,
    host_id: String,
    cipher: XChaCha20Poly1305,
    provider: tokio::sync::OnceCell<Provider>,
    /// The sign-ins under way, by their `state`.
    pending: Mutex<HashMap<String, Pending>>,
    /// Each link's access token, while it lasts: held while it's renewed.
    access: Mutex<HashMap<String, Arc<tokio::sync::Mutex<Option<Access>>>>>,
}

/// OpenAI's endpoints, from its discovery document, and its keys.
struct Provider {
    issuer: String,
    authorize: String,
    token: String,
    revocation: Option<String>,
    keys: Arc<Keys>,
}

struct Pending {
    owner: String,
    verifier: String,
    nonce: String,
    redirect: String,
    /// The client its link was registered with before; none, a first sign-in.
    client_id: Option<String>,
    at: Instant,
}

#[derive(Clone)]
struct Access {
    token: String,
    expires: Instant,
    /// Not renewed before this (OpenAI's `earliest_refresh_at`).
    renew_after: Option<Instant>,
}

/// A link's state, as its user sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LinkInfo {
    pub email: String,
    pub broken: Option<String>,
}

/// A sign-in to make in the browser.
#[derive(Debug, Serialize, Deserialize)]
pub struct Started {
    pub url: String,
    pub state: String,
}

/// A model the plan has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Model {
    pub slug: String,
    pub display_name: String,
}

#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    jwks_uri: String,
    revocation_endpoint: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
    /// Seconds since the epoch.
    earliest_refresh_at: Option<f64>,
}

impl ChatGpt {
    /// Without a link key, none.
    pub fn new(args: &ChatGptArgs, host_key_id: &str) -> anyhow::Result<Option<ChatGpt>> {
        let Some(path) = &args.link_key else {
            return Ok(None);
        };
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Some(ChatGpt {
            http,
            auth: args.openai_auth.trim_end_matches('/').into(),
            api: args.openai_api.trim_end_matches('/').into(),
            host_id: format!("urn:ietf:params:oauth:jwk-thumbprint:sha-256:{host_key_id}"),
            cipher: XChaCha20Poly1305::new(&load_key(path)?.into()),
            provider: Default::default(),
            pending: Default::default(),
            access: Default::default(),
        }))
    }

    async fn provider(&self) -> anyhow::Result<&Provider> {
        self.provider
            .get_or_try_init(|| async {
                let url = format!("{}/.well-known/openid-configuration", self.auth);
                let d: Discovery = self
                    .http
                    .get(&url)
                    .send()
                    .await
                    .and_then(|r| r.error_for_status())
                    .with_context(|| format!("fetching {url}"))?
                    .json()
                    .await
                    .with_context(|| format!("{url}: not a discovery document"))?;
                Ok(Provider {
                    keys: Keys::start(self.http.clone(), &d.jwks_uri).await?,
                    issuer: d.issuer,
                    authorize: d.authorization_endpoint,
                    token: d.token_endpoint,
                    revocation: d.revocation_endpoint,
                })
            })
            .await
    }

    /// A sign-in for `owner`, coming back to the loopback on `port`.
    pub async fn start(&self, store: &Store, owner: &str, port: u16) -> anyhow::Result<Started> {
        let provider = self.provider().await?;
        let link = store.link(owner).await?;
        let state = random();
        let verifier = random();
        let nonce = random();
        let redirect = format!("http://127.0.0.1:{port}/auth/callback");
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let mut params = vec![
            ("response_type", "code"),
            ("redirect_uri", redirect.as_str()),
            ("scope", SCOPES),
            ("resource", RESOURCE),
            ("state", state.as_str()),
            ("nonce", nonce.as_str()),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("ext_agent_host_id", self.host_id.as_str()),
        ];
        // signing in again: as the client registered before, for the same account
        match &link {
            Some(link) => params.extend([
                ("client_id", link.client_id.as_str()),
                ("id_token_hint", link.id_token.as_str()),
                ("login_hint", link.email.as_str()),
            ]),
            None => params.extend([
                ("client_id", DYNAMIC_CLIENT),
                ("agent_name_hint", AGENT_NAME),
            ]),
        }
        let url = Url::parse_with_params(&provider.authorize, &params)?;
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|_, p| p.at.elapsed() < PENDING);
        pending.insert(
            state.clone(),
            Pending {
                owner: owner.into(),
                verifier,
                nonce,
                redirect,
                client_id: link.map(|l| l.client_id),
                at: Instant::now(),
            },
        );
        Ok(Started {
            url: url.into(),
            state,
        })
    }

    /// The sign-in `state` came back with `code` (and, a first one, the `client_id` OpenAI
    /// registered): redeemed, it's `owner`'s link.
    pub async fn finish(
        &self,
        store: &Store,
        owner: &str,
        code: &str,
        state: &str,
        client_id: Option<&str>,
    ) -> Result<LinkInfo, String> {
        let pending = self.pending.lock().unwrap().remove(state);
        let pending = match pending {
            Some(p) if p.owner == owner && p.at.elapsed() < PENDING => p,
            _ => return Err("no such sign-in under way (start it again)".into()),
        };
        let client_id = match (&pending.client_id, client_id) {
            (Some(saved), Some(got)) if saved != got => {
                return Err("the sign-in came back for another client".into())
            }
            (Some(saved), _) => saved.clone(),
            (None, Some(got)) => got.to_string(),
            (None, None) => return Err("OpenAI registered no client for the sign-in".into()),
        };
        let provider = self.provider().await.map_err(|e| format!("{e:#}"))?;
        let tokens = self
            .token(
                &provider.token,
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", &client_id),
                    ("code", code),
                    ("code_verifier", &pending.verifier),
                    ("redirect_uri", &pending.redirect),
                    ("resource", RESOURCE),
                ],
            )
            .await
            .map_err(|e| match e {
                TokenError::Refused { code, message } => {
                    format!("OpenAI refused it: {code} {message}")
                }
                TokenError::Failed(e) => format!("{e:#}"),
            })?;
        let granted = tokens.scope.as_deref().unwrap_or_default();
        if !granted.split(' ').any(|s| s == PLAN_SCOPE) {
            return Err("signed in, but using the ChatGPT plan wasn't allowed".into());
        }
        let id_token = tokens
            .id_token
            .as_deref()
            .ok_or("OpenAI sent no ID token")?;
        let claims = verify_id_token(provider, id_token, &client_id, &pending.nonce).await?;
        let account = claims
            .get("sub")
            .and_then(Value::as_str)
            .ok_or("the ID token has no sub")?;
        let email = claims
            .get("email")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let refresh = tokens
            .refresh_token
            .as_deref()
            .ok_or("OpenAI sent no refresh token")?;
        let link = Link {
            account: account.into(),
            email: email.into(),
            client_id,
            id_token: id_token.into(),
            refresh_token: self.seal(refresh.as_bytes()),
            broken: None,
        };
        store
            .put_link(owner, &link)
            .await
            .map_err(|e| format!("keeping the link: {e:#}"))?;
        *self.slot(owner).lock().await = Some(access(&tokens));
        info!(owner, account, "ChatGPT linked");
        Ok(LinkInfo {
            email: link.email,
            broken: None,
        })
    }

    /// `owner`'s link, as they see it.
    pub async fn info(&self, store: &Store, owner: &str) -> anyhow::Result<Option<LinkInfo>> {
        Ok(store.link(owner).await?.map(|l| LinkInfo {
            email: l.email,
            broken: l.broken,
        }))
    }

    fn slot(&self, owner: &str) -> Arc<tokio::sync::Mutex<Option<Access>>> {
        self.access
            .lock()
            .unwrap()
            .entry(owner.into())
            .or_default()
            .clone()
    }

    /// An access token of `owner`'s plan, with at least [`RENEW_BEFORE`] left: renewed if it
    /// hasn't, one renewal at a time. `rejected`: OpenAI refused the one handed out last, which
    /// isn't handed out again.
    pub async fn access(
        &self,
        store: &Store,
        owner: &str,
        rejected: bool,
    ) -> Result<String, Unavailable> {
        let slot = self.slot(owner);
        let mut cached = slot.lock().await;
        if rejected {
            *cached = None;
        }
        if let Some(a) = cached.as_ref() {
            let now = Instant::now();
            let fresh = a.expires > now + RENEW_BEFORE;
            let not_yet = a.renew_after.is_some_and(|t| now < t) && a.expires > now;
            if fresh || not_yet {
                return Ok(a.token.clone());
            }
        }
        let link = store
            .link(owner)
            .await
            .map_err(Unavailable::Failed)?
            .ok_or(Unavailable::NotLinked)?;
        if let Some(why) = link.broken {
            return Err(Unavailable::Broken(why));
        }
        let refresh = self
            .open(&link.refresh_token)
            .map_err(Unavailable::Failed)?;
        let provider = self.provider().await.map_err(Unavailable::Failed)?;
        let renewed = self.renew(&provider.token, &link.client_id, &refresh).await;
        match renewed {
            Ok(tokens) => {
                if let Some(new) = &tokens.refresh_token {
                    let sealed = self.seal(new.as_bytes());
                    let kept = store
                        .renewed_link(owner, &link.refresh_token, Ok(&sealed))
                        .await
                        .map_err(Unavailable::Failed)?;
                    if !kept {
                        warn!(owner, "the ChatGPT link changed while it was renewed");
                    }
                }
                let a = access(&tokens);
                *cached = Some(a.clone());
                Ok(a.token)
            }
            Err(TokenError::Refused { code, message }) if TERMINAL.contains(&code.as_str()) => {
                let why = if message.is_empty() {
                    code
                } else {
                    format!("{code}: {message}")
                };
                warn!(owner, why, "the ChatGPT link no longer renews");
                store
                    .renewed_link(owner, &link.refresh_token, Err(&why))
                    .await
                    .map_err(Unavailable::Failed)?;
                Err(Unavailable::Broken(why))
            }
            Err(TokenError::Refused { code, message }) => Err(Unavailable::Failed(
                anyhow::anyhow!("OpenAI refused the renewal: {code} {message}"),
            )),
            Err(TokenError::Failed(e)) => Err(Unavailable::Failed(e)),
        }
    }

    /// The refresh token's renewal, tried again (bounded) while OpenAI can't be reached or
    /// fails.
    async fn renew(
        &self,
        endpoint: &str,
        client_id: &str,
        refresh: &str,
    ) -> Result<TokenResponse, TokenError> {
        let mut delay = Duration::from_millis(500);
        let mut tries = 0;
        loop {
            let got = self
                .token(
                    endpoint,
                    &[
                        ("grant_type", "refresh_token"),
                        ("client_id", client_id),
                        ("refresh_token", refresh),
                        ("resource", RESOURCE),
                    ],
                )
                .await;
            tries += 1;
            match got {
                Err(TokenError::Failed(e)) if tries < 4 => {
                    warn!("renewing a ChatGPT link (trying again in {delay:?}): {e:#}");
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
                got => return got,
            }
        }
    }

    async fn token(
        &self,
        endpoint: &str,
        form: &[(&str, &str)],
    ) -> Result<TokenResponse, TokenError> {
        let res = self
            .http
            .post(endpoint)
            .form(form)
            .send()
            .await
            .map_err(|e| TokenError::Failed(e.into()))?;
        let status = res.status();
        if status.is_success() {
            return res
                .json()
                .await
                .map_err(|e| TokenError::Failed(anyhow::anyhow!("OpenAI's token response: {e}")));
        }
        let body = res.text().await.unwrap_or_default();
        if status.is_server_error() {
            return Err(TokenError::Failed(anyhow::anyhow!(
                "OpenAI: {status} {body}"
            )));
        }
        let (code, message) = oauth_error(&body);
        Err(TokenError::Refused {
            code: code.unwrap_or_else(|| status.as_u16().to_string()),
            message,
        })
    }

    /// `owner`'s link revoked at OpenAI (as best it can be) and forgotten; whether there was one.
    pub async fn unlink(&self, store: &Store, owner: &str) -> anyhow::Result<bool> {
        let slot = self.slot(owner);
        let mut cached = slot.lock().await;
        let Some(link) = store.link(owner).await? else {
            return Ok(false);
        };
        let refresh = self.open(&link.refresh_token)?;
        if let Some(endpoint) = &self.provider().await?.revocation {
            let res = self
                .http
                .post(endpoint)
                .form(&[
                    ("token", refresh.as_str()),
                    ("token_type_hint", "refresh_token"),
                    ("client_id", link.client_id.as_str()),
                ])
                .send()
                .await;
            match res {
                Ok(r) if r.status().is_success() => {}
                Ok(r) => {
                    warn!(owner, status = %r.status(), "OpenAI didn't revoke the ChatGPT link")
                }
                Err(e) => warn!(owner, "revoking the ChatGPT link: {e}"),
            }
        }
        *cached = None;
        store.delete_link(owner).await?;
        info!(owner, "ChatGPT unlinked");
        Ok(true)
    }

    /// The models `owner`'s plan has, as OpenAI lists them to show.
    pub async fn models(&self, store: &Store, owner: &str) -> Result<Vec<Model>, String> {
        let token = self
            .access(store, owner, false)
            .await
            .map_err(|e| e.to_string())?;
        #[derive(Deserialize)]
        struct Listed {
            slug: String,
            display_name: Option<String>,
            visibility: Option<String>,
        }
        #[derive(Deserialize)]
        struct List {
            models: Vec<Listed>,
        }
        let url = format!("{}/models", self.api);
        let res = self
            .http
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|e| format!("listing the plan's models: {e}"))?;
        let status = res.status();
        if !status.is_success() {
            let body = res.text().await.unwrap_or_default();
            return Err(format!("listing the plan's models: {status} {body}"));
        }
        let list: List = res
            .json()
            .await
            .map_err(|e| format!("listing the plan's models: {e}"))?;
        Ok(list
            .models
            .into_iter()
            .filter(|m| m.visibility.as_deref().is_none_or(|v| v == "list"))
            .map(|m| Model {
                display_name: m.display_name.unwrap_or_else(|| m.slug.clone()),
                slug: m.slug,
            })
            .collect())
    }

    fn seal(&self, plain: &[u8]) -> Vec<u8> {
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let mut sealed = nonce.to_vec();
        sealed.extend(
            self.cipher
                .encrypt(&nonce, plain)
                .expect("encrypting in memory"),
        );
        sealed
    }

    fn open(&self, sealed: &[u8]) -> anyhow::Result<String> {
        anyhow::ensure!(sealed.len() > 24, "a sealed refresh token too short");
        let (nonce, text) = sealed.split_at(24);
        let plain = self
            .cipher
            .decrypt(XNonce::from_slice(nonce), text)
            .map_err(|_| anyhow::anyhow!("the refresh token isn't the link key's"))?;
        Ok(String::from_utf8(plain)?)
    }
}

enum TokenError {
    /// OpenAI's OAuth error.
    Refused { code: String, message: String },
    /// It couldn't be reached, or it failed.
    Failed(anyhow::Error),
}

/// An OAuth error body's code and message: `{"error": "…", "error_description": "…"}`, or
/// `{"error": {"code": "…", "message": "…"}}`.
fn oauth_error(body: &str) -> (Option<String>, String) {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return (None, body.chars().take(200).collect());
    };
    let error = &v["error"];
    let code = error
        .as_str()
        .or_else(|| error["code"].as_str())
        .map(String::from);
    let message = v["error_description"]
        .as_str()
        .or_else(|| error["message"].as_str())
        .unwrap_or_default()
        .to_string();
    (code, message)
}

fn access(tokens: &TokenResponse) -> Access {
    let now = Instant::now();
    let renew_after = tokens.earliest_refresh_at.and_then(|at| {
        let unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_secs_f64();
        (at > unix).then(|| now + Duration::from_secs_f64(at - unix))
    });
    Access {
        token: tokens.access_token.clone(),
        expires: now + Duration::from_secs(tokens.expires_in.unwrap_or(3600)),
        renew_after,
    }
}

/// The ID token's claims, verified: OpenAI's signature, issuer, audience (the client) and the
/// sign-in's nonce.
async fn verify_id_token(
    provider: &Provider,
    token: &str,
    client_id: &str,
    nonce: &str,
) -> Result<Map<String, Value>, String> {
    let header = asymmetric_header(token)?;
    let key = provider.keys.decoding(header.kid.as_deref()).await?;
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[&provider.issuer]);
    validation.set_audience(&[client_id]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    validation.leeway = LEEWAY;
    let claims = jsonwebtoken::decode::<Map<String, Value>>(token, &key, &validation)
        .map_err(|e| format!("OpenAI's ID token isn't taken: {e}"))?
        .claims;
    if claims.get("nonce").and_then(Value::as_str) != Some(nonce) {
        return Err("OpenAI's ID token isn't this sign-in's".into());
    }
    Ok(claims)
}

fn random() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn load_key(path: &Path) -> anyhow::Result<[u8; 32]> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the link key {}", path.display()))?;
    let bytes = STANDARD
        .decode(text.trim())
        .with_context(|| format!("{}: not base64", path.display()))?;
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("{}: not 32 bytes", path.display()))
}

/// A new link key in `path` (32 random bytes, base64), unless there's one.
pub fn keygen(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        return Ok(());
    }
    let mut key = [0u8; 32];
    rand::rng().fill_bytes(&mut key);
    crate::token::write_private(path, STANDARD.encode(key).as_bytes())
        .with_context(|| format!("writing {}", path.display()))
}

/// OpenAI's side of Sign in with ChatGPT, for the tests: discovery, authorize (which redirects
/// at once, as a user who's signed in and consents), the token endpoint (codes, and refresh
/// tokens that rotate, an old one refused as reused), revocation, its keys, the plan's models,
/// and the Responses API, streaming a reply as the access token's (or failing, as told).
#[cfg(test)]
pub mod fake {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::extract::{Form, Query, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Redirect, Response};
    use axum::routing::{get, post};
    use axum::{Json, Router};

    use super::*;
    use crate::token::{test_signer, Signer};

    pub struct OpenAi {
        pub url: String,
        pub state: &'static FakeState,
    }

    pub struct FakeState {
        url: Mutex<String>,
        signer: Signer,
        /// code → (client, nonce, challenge, redirect)
        codes: Mutex<HashMap<String, (String, String, String, String)>>,
        /// The refresh token that renews, and the access token that's taken.
        pub refresh: Mutex<Option<String>>,
        pub access: Mutex<Option<String>>,
        pub refreshes: AtomicUsize,
        pub revoked: AtomicUsize,
        /// What the next Responses calls fail with: (status, error code).
        pub fail: Mutex<Option<(u16, String)>>,
        /// What the Responses calls were sent.
        pub requests: Mutex<Vec<Value>>,
        n: AtomicUsize,
    }

    pub const CLIENT: &str = "oaiapp_test";
    pub const ACCOUNT: &str = "user-chatgpt-1";
    pub const EMAIL: &str = "plan@example.com";

    impl OpenAi {
        pub async fn start() -> OpenAi {
            let state: &'static FakeState = Box::leak(Box::new(FakeState {
                url: Default::default(),
                signer: test_signer("fake-openai", Duration::from_secs(3600)),
                codes: Default::default(),
                refresh: Default::default(),
                access: Default::default(),
                refreshes: AtomicUsize::new(0),
                revoked: AtomicUsize::new(0),
                fail: Default::default(),
                requests: Default::default(),
                n: AtomicUsize::new(0),
            }));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            *state.url.lock().unwrap() = url.clone();
            let app = Router::new()
                .route("/.well-known/openid-configuration", get(discovery))
                .route("/jwks", get(jwks))
                .route("/authorize", get(authorize))
                .route("/token", post(token))
                .route("/revoke", post(revoke))
                .route("/v1/models", get(models))
                .route("/v1/responses", post(responses))
                .with_state(state);
            tokio::spawn(async move { axum::serve(listener, app).await });
            OpenAi { url, state }
        }

        pub fn args(&self, link_key: &Path) -> ChatGptArgs {
            ChatGptArgs {
                link_key: Some(link_key.into()),
                openai_auth: self.url.clone(),
                openai_api: format!("{}/v1", self.url),
            }
        }
    }

    impl FakeState {
        fn next(&self, what: &str) -> String {
            format!("{what}-{}", self.n.fetch_add(1, Ordering::SeqCst))
        }

        fn issue(&self) -> Value {
            let access = self.next("at");
            let refresh = self.next("rt");
            *self.access.lock().unwrap() = Some(access.clone());
            *self.refresh.lock().unwrap() = Some(refresh.clone());
            serde_json::json!({
                "access_token": access,
                "refresh_token": refresh,
                "token_type": "Bearer",
                "expires_in": 3600,
                "scope": SCOPES,
            })
        }
    }

    async fn discovery(State(s): State<&'static FakeState>) -> Json<Value> {
        let url = s.url.lock().unwrap().clone();
        Json(serde_json::json!({
            "issuer": url,
            "authorization_endpoint": format!("{url}/authorize"),
            "token_endpoint": format!("{url}/token"),
            "jwks_uri": format!("{url}/jwks"),
            "revocation_endpoint": format!("{url}/revoke"),
        }))
    }

    async fn jwks(State(s): State<&'static FakeState>) -> Json<Value> {
        Json(s.signer.jwks())
    }

    async fn authorize(
        State(s): State<&'static FakeState>,
        Query(q): Query<HashMap<String, String>>,
    ) -> Response {
        let client = match q.get("client_id").map(String::as_str) {
            Some(DYNAMIC_CLIENT) | Some(CLIENT) => CLIENT,
            _ => return StatusCode::BAD_REQUEST.into_response(),
        };
        assert!(q.contains_key("ext_agent_host_id"));
        let code = s.next("code");
        s.codes.lock().unwrap().insert(
            code.clone(),
            (
                client.into(),
                q["nonce"].clone(),
                q["code_challenge"].clone(),
                q["redirect_uri"].clone(),
            ),
        );
        let mut back = Url::parse(&q["redirect_uri"]).unwrap();
        back.query_pairs_mut()
            .append_pair("code", &code)
            .append_pair("state", &q["state"])
            .append_pair("scope", SCOPES);
        if q["client_id"] == DYNAMIC_CLIENT {
            back.query_pairs_mut().append_pair("client_id", CLIENT);
        }
        Redirect::to(back.as_str()).into_response()
    }

    fn refused(code: &str) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": code, "error_description": "refused" })),
        )
            .into_response()
    }

    async fn token(
        State(s): State<&'static FakeState>,
        Form(f): Form<HashMap<String, String>>,
    ) -> Response {
        if f.get("client_id").map(String::as_str) != Some(CLIENT) {
            return refused("invalid_client");
        }
        match f.get("grant_type").map(String::as_str) {
            Some("authorization_code") => {
                let Some((_, nonce, challenge, redirect)) =
                    s.codes.lock().unwrap().remove(&f["code"])
                else {
                    return refused("invalid_grant");
                };
                let verified =
                    URL_SAFE_NO_PAD.encode(Sha256::digest(f["code_verifier"].as_bytes()));
                if verified != challenge || f["redirect_uri"] != redirect {
                    return refused("invalid_grant");
                }
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();
                let id_token = s.signer.sign_claims(&serde_json::json!({
                    "iss": s.url.lock().unwrap().clone(), "aud": CLIENT, "sub": ACCOUNT,
                    "email": EMAIL, "nonce": nonce, "iat": now, "exp": now + 3600,
                }));
                let mut issued = s.issue();
                issued["id_token"] = id_token.into();
                Json(issued).into_response()
            }
            Some("refresh_token") => {
                let current = s.refresh.lock().unwrap().clone();
                if current.as_deref() != Some(f["refresh_token"].as_str()) {
                    return refused("refresh_token_reused");
                }
                s.refreshes.fetch_add(1, Ordering::SeqCst);
                // a renewal takes a while: renewals that race would both be under way
                tokio::time::sleep(Duration::from_millis(50)).await;
                Json(s.issue()).into_response()
            }
            _ => refused("unsupported_grant_type"),
        }
    }

    async fn revoke(State(s): State<&'static FakeState>) -> StatusCode {
        s.revoked.fetch_add(1, Ordering::SeqCst);
        *s.refresh.lock().unwrap() = None;
        StatusCode::OK
    }

    fn authorized(s: &FakeState, headers: &HeaderMap) -> bool {
        let bearer = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "));
        bearer.is_some() && bearer == s.access.lock().unwrap().as_deref()
    }

    async fn models(State(s): State<&'static FakeState>, headers: HeaderMap) -> Response {
        if !authorized(s, &headers) {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        Json(serde_json::json!({ "models": [
            { "slug": "gpt-test", "display_name": "GPT Test", "visibility": "list" },
            { "slug": "gpt-hidden", "display_name": "Hidden", "visibility": "hide" },
        ]}))
        .into_response()
    }

    /// One reply's text, streamed as the Responses API does.
    pub const REPLY: &str = "Hello from the plan.";

    async fn responses(
        State(s): State<&'static FakeState>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Response {
        s.requests.lock().unwrap().push(body.clone());
        if !authorized(s, &headers) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({"detail": "no"})),
            )
                .into_response();
        }
        if let Some((status, code)) = s.fail.lock().unwrap().clone() {
            let error =
                serde_json::json!({ "error": { "code": code, "message": code, "type": "x" } });
            return (StatusCode::from_u16(status).unwrap(), Json(error)).into_response();
        }
        let model = body["model"].as_str().unwrap_or_default().to_string();
        let item = serde_json::json!({
            "id": "msg_1", "type": "message", "role": "assistant", "status": "completed",
            "content": [{ "type": "output_text", "text": REPLY, "annotations": [] }],
        });
        let events = [
            serde_json::json!({ "type": "response.created",
                "response": { "id": "resp_1", "model": model, "status": "in_progress", "output": [] } }),
            serde_json::json!({ "type": "response.output_item.added", "output_index": 0,
                "item": { "id": "msg_1", "type": "message", "role": "assistant", "status": "in_progress", "content": [] } }),
            serde_json::json!({ "type": "response.output_text.delta", "item_id": "msg_1",
                "output_index": 0, "content_index": 0, "delta": REPLY }),
            serde_json::json!({ "type": "response.output_item.done", "output_index": 0, "item": item }),
            serde_json::json!({ "type": "response.completed", "response": {
                "id": "resp_1", "model": model, "status": "completed", "output": [item],
                "usage": { "input_tokens": 10, "output_tokens": 5, "total_tokens": 15,
                    "input_tokens_details": { "cached_tokens": 0 },
                    "output_tokens_details": { "reasoning_tokens": 0 } } } }),
        ];
        let sse: String = events
            .iter()
            .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
            .collect();
        ([("content-type", "text/event-stream")], sse).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chatgpt(dir: &Path) -> ChatGpt {
        let key = dir.join("link.key");
        keygen(&key).unwrap();
        let args = ChatGptArgs {
            link_key: Some(key),
            openai_auth: "http://127.0.0.1:1".into(),
            openai_api: "http://127.0.0.1:1/v1".into(),
        };
        ChatGpt::new(&args, "kid").unwrap().unwrap()
    }

    #[test]
    fn a_sealed_refresh_token_opens_only_with_its_key() {
        let dir = tempfile::tempdir().unwrap();
        let c = chatgpt(dir.path());
        let sealed = c.seal(b"rt-secret");
        assert!(!sealed.windows(9).any(|w| w == b"rt-secret"));
        assert_ne!(sealed, c.seal(b"rt-secret"), "a nonce of its own each time");
        assert_eq!(c.open(&sealed).unwrap(), "rt-secret");
        let other = tempfile::tempdir().unwrap();
        assert!(chatgpt(other.path()).open(&sealed).is_err());
        assert!(c.open(&sealed[..10]).is_err());
    }

    #[test]
    fn the_host_is_named_by_controls_key_and_keygen_keeps_a_key() {
        let dir = tempfile::tempdir().unwrap();
        let c = chatgpt(dir.path());
        assert_eq!(
            c.host_id,
            "urn:ietf:params:oauth:jwk-thumbprint:sha-256:kid"
        );
        let key = dir.path().join("link.key");
        let before = std::fs::read(&key).unwrap();
        keygen(&key).unwrap();
        assert_eq!(std::fs::read(&key).unwrap(), before);
        assert_eq!(load_key(&key).unwrap().len(), 32);
    }

    #[test]
    fn oauth_errors_are_read_in_either_shape() {
        assert_eq!(
            oauth_error(r#"{"error":"refresh_token_reused","error_description":"used"}"#),
            (Some("refresh_token_reused".into()), "used".into())
        );
        assert_eq!(
            oauth_error(r#"{"error":{"code":"invalid_grant","message":"m"}}"#),
            (Some("invalid_grant".into()), "m".into())
        );
        assert_eq!(oauth_error("<html>"), (None, "<html>".into()));
    }

    #[test]
    fn a_renewal_waits_for_earliest_refresh_at() {
        let unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        let t = TokenResponse {
            access_token: "a".into(),
            refresh_token: None,
            id_token: None,
            expires_in: Some(60),
            scope: None,
            earliest_refresh_at: Some(unix + 30.0),
        };
        let a = access(&t);
        assert!(a.renew_after.unwrap() > Instant::now() + Duration::from_secs(25));
        assert!(a.expires < Instant::now() + Duration::from_secs(61));
    }
}
