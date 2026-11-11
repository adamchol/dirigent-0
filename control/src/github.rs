//! GitHub, for sessions on its repositories: whether one can be read, and how (`/github/access`:
//! it's public, or the caller's `GITHUB_TOKEN` secret or the GitHub App reaches it); and, when
//! the install has a GitHub App (optional), tokens of it for a session's repository, which its
//! player gets as its `GITHUB_TOKEN` unless the session has one of its own.
//!
//! A user connects GitHub by installing the App (or authorizing it, for installations made
//! already): GitHub sends their browser back to control with a code, which shows which
//! installations their GitHub account reaches; those are theirs to use from then on. Each
//! session's token is for its one repository, from the installation on the account that owns it,
//! and lasts an hour: control mints another when it has less than [`RENEW_BEFORE`] left, and the
//! leases send them on as they renew their credentials.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::leases::Store;

/// The secret a session's git and `gh` use for GitHub.
pub const GITHUB_TOKEN: &str = "GITHUB_TOKEN";
/// The cookie that ties a user's connecting GitHub to their browser: its value's hash is in
/// the `state` control sends GitHub with them ([`link_audience`]), so a `state` someone else
/// started, and tricked them into taking to GitHub, doesn't link their installations to that
/// someone. `SameSite=Lax`: GitHub sends the browser back from its site.
pub const COOKIE: &str = "dirigent_github";
/// How long an installation's token lasts (GitHub's hour, as it was minted).
const TOKEN_LIFE: Duration = Duration::from_secs(3600);
/// A token with less left than this is minted again: the leases' credentials are sent afresh
/// every few minutes, so the player's never runs out.
const RENEW_BEFORE: Duration = Duration::from_secs(15 * 60);

#[derive(clap::Args, Debug, Clone)]
pub struct GithubArgs {
    /// GitHub's API.
    #[arg(
        long,
        env = "DIRIGENT_GITHUB_API",
        default_value = "https://api.github.com"
    )]
    pub github_api: String,

    /// GitHub's site, where its users install the App and authorize it.
    #[arg(
        long,
        env = "DIRIGENT_GITHUB_WEB",
        default_value = "https://github.com"
    )]
    pub github_web: String,

    /// The GitHub App's name in its URL (`github.com/apps/<slug>`). The App is optional; with
    /// it, all of its `--github-app-*` are needed.
    #[arg(long, env = "DIRIGENT_GITHUB_APP_SLUG")]
    pub github_app_slug: Option<String>,

    /// Its client ID.
    #[arg(long, env = "DIRIGENT_GITHUB_APP_CLIENT_ID")]
    pub github_app_client_id: Option<String>,

    /// Its client secret.
    #[arg(long, env = "DIRIGENT_GITHUB_APP_CLIENT_SECRET")]
    pub github_app_client_secret: Option<String>,

    /// Its private key (the PEM file GitHub gave).
    #[arg(long, env = "DIRIGENT_GITHUB_APP_KEY")]
    pub github_app_key: Option<PathBuf>,
}

/// A repository on github.com.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub owner: String,
    pub name: String,
}

impl Repo {
    /// The github.com repository a git URL is, as https, `git@` or `ssh://`; none for other
    /// hosts or what isn't one.
    pub fn parse(url: &str) -> Option<Repo> {
        let path = [
            "https://github.com/",
            "https://www.github.com/",
            "http://github.com/",
            "git@github.com:",
            "ssh://git@github.com/",
            "git://github.com/",
        ]
        .iter()
        .find_map(|p| url.trim().strip_prefix(p))?;
        let path = path.trim_end_matches('/');
        let path = path.strip_suffix(".git").unwrap_or(path);
        let (owner, name) = path.split_once('/')?;
        let ok = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        (ok(owner) && ok(name)).then(|| Repo {
            owner: owner.into(),
            name: name.into(),
        })
    }

    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

pub struct Github {
    api: String,
    web: String,
    http: reqwest::Client,
    app: Option<App>,
}

struct App {
    slug: String,
    client_id: String,
    client_secret: String,
    key: EncodingKey,
    /// The tokens minted, by installation and repository (lowercase): held while one's minted.
    tokens: std::sync::Mutex<HashMap<(i64, String), Slot>>,
}

/// A token of the App's, once minted.
type Slot = Arc<tokio::sync::Mutex<Option<Minted>>>;

struct Minted {
    token: String,
    until: Instant,
}

/// How a user's GitHub account reaches an installation (`GET /user/installations`).
#[derive(Deserialize)]
struct Installations {
    installations: Vec<Installation>,
}

#[derive(Deserialize)]
struct Installation {
    id: i64,
    account: Account,
}

#[derive(Deserialize)]
struct Account {
    login: String,
}

impl Github {
    pub fn new(args: &GithubArgs) -> anyhow::Result<Github> {
        let app = match (
            &args.github_app_slug,
            &args.github_app_client_id,
            &args.github_app_client_secret,
            &args.github_app_key,
        ) {
            (None, None, None, None) => None,
            (Some(slug), Some(client_id), Some(client_secret), Some(key)) => {
                let pem = std::fs::read(key)
                    .with_context(|| format!("reading the GitHub App's key {}", key.display()))?;
                Some(App {
                    slug: slug.clone(),
                    client_id: client_id.clone(),
                    client_secret: client_secret.clone(),
                    key: EncodingKey::from_rsa_pem(&pem).with_context(|| {
                        format!("{}: not the GitHub App's RSA key", key.display())
                    })?,
                    tokens: Default::default(),
                })
            }
            _ => anyhow::bail!(
                "a GitHub App needs all of --github-app-slug, --github-app-client-id, \
                 --github-app-client-secret and --github-app-key"
            ),
        };
        Ok(Github {
            api: args.github_api.trim_end_matches('/').into(),
            web: args.github_web.trim_end_matches('/').into(),
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .user_agent("dirigent")
                .build()?,
            app,
        })
    }

    /// Whether the install has a GitHub App.
    pub fn has_app(&self) -> bool {
        self.app.is_some()
    }

    /// Where a user connects GitHub, `state` coming back with them: installing the App on an
    /// account, or (`existing`) authorizing it, for the installations they reach already.
    pub fn connect_url(&self, state: &str, existing: bool) -> Option<String> {
        let app = self.app.as_ref()?;
        let mut url = if existing {
            let mut url =
                reqwest::Url::parse(&format!("{}/login/oauth/authorize", self.web)).ok()?;
            url.query_pairs_mut()
                .append_pair("client_id", &app.client_id);
            url
        } else {
            reqwest::Url::parse(&format!("{}/apps/{}/installations/new", self.web, app.slug))
                .ok()?
        };
        url.query_pairs_mut().append_pair("state", state);
        Some(url.into())
    }

    /// The installations (id, account) of the App the GitHub user who authorized it (`code`,
    /// from GitHub's redirect) reaches: theirs to use. Their GitHub token isn't kept.
    pub async fn installations(&self, code: &str) -> Result<Vec<(i64, String)>, String> {
        let app = self.app.as_ref().ok_or("this control has no GitHub App")?;
        #[derive(Deserialize)]
        struct Exchanged {
            access_token: Option<String>,
            error_description: Option<String>,
        }
        let exchanged: Exchanged = self
            .http
            .post(format!("{}/login/oauth/access_token", self.web))
            .header("accept", "application/json")
            .form(&[
                ("client_id", app.client_id.as_str()),
                ("client_secret", app.client_secret.as_str()),
                ("code", code),
            ])
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("GitHub didn't take the authorization: {e}"))?
            .json()
            .await
            .map_err(|e| format!("GitHub's answer to the authorization: {e}"))?;
        let user = exchanged.access_token.ok_or_else(|| {
            format!(
                "GitHub didn't take the authorization: {}",
                exchanged
                    .error_description
                    .as_deref()
                    .unwrap_or("no reason")
            )
        })?;
        let found: Installations = self
            .get(&format!("{}/user/installations?per_page=100", self.api))
            .bearer_auth(user)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| format!("reading your GitHub account's installations: {e}"))?
            .json()
            .await
            .map_err(|e| format!("reading your GitHub account's installations: {e}"))?;
        Ok(found
            .installations
            .into_iter()
            .map(|i| (i.id, i.account.login))
            .collect())
    }

    /// Whether `repo` can be read, anonymously or with `token`.
    pub async fn readable(&self, repo: &Repo, token: Option<&str>) -> anyhow::Result<bool> {
        let mut req = self.get(&format!("{}/repos/{}", self.api, repo.full_name()));
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        let status = req.send().await?.status();
        match status.as_u16() {
            200 => Ok(true),
            401 | 404 => Ok(false),
            _ => anyhow::bail!("GitHub answered {status}"),
        }
    }

    /// A token of the App for `repo` alone, from the installation on its owner's account that
    /// `subject` may use: none without one (or without an App); why not, if GitHub refuses it
    /// (the installation doesn't include it).
    pub async fn token(
        &self,
        store: &Store,
        subject: &str,
        repo: &Repo,
    ) -> Result<Option<String>, String> {
        let Some(app) = &self.app else {
            return Ok(None);
        };
        let installations = store
            .github_installations(subject)
            .await
            .map_err(|e| format!("reading the GitHub installations: {e:#}"))?;
        let Some((id, _)) = installations
            .iter()
            .find(|(_, account)| account.eq_ignore_ascii_case(&repo.owner))
        else {
            return Ok(None);
        };
        let slot = app
            .tokens
            .lock()
            .unwrap()
            .entry((*id, repo.full_name().to_lowercase()))
            .or_default()
            .clone();
        let mut minted = slot.lock().await;
        if let Some(m) = minted
            .as_ref()
            .filter(|m| m.until > Instant::now() + RENEW_BEFORE)
        {
            return Ok(Some(m.token.clone()));
        }
        let started = Instant::now();
        let token = self.mint(app, *id, repo).await?;
        *minted = Some(Minted {
            token: token.clone(),
            until: started + TOKEN_LIFE,
        });
        Ok(Some(token))
    }

    async fn mint(&self, app: &App, installation: i64, repo: &Repo) -> Result<String, String> {
        #[derive(Deserialize)]
        struct Minted {
            token: String,
        }
        let res = self
            .http
            .post(format!(
                "{}/app/installations/{installation}/access_tokens",
                self.api
            ))
            .header("accept", "application/vnd.github+json")
            .bearer_auth(app_jwt(app))
            .json(&serde_json::json!({ "repositories": [repo.name] }))
            .send()
            .await
            .map_err(|e| format!("asking GitHub for a token: {e}"))?;
        match res.status().as_u16() {
            201 | 200 => {}
            404 | 422 => {
                return Err(format!(
                    "the GitHub App's installation on {} doesn't include {}: add it to the \
                     installation's repositories on GitHub",
                    repo.owner,
                    repo.full_name()
                ))
            }
            _ => return Err(format!("GitHub refused a token: {}", res.status())),
        }
        res.json::<Minted>()
            .await
            .map(|m| m.token)
            .map_err(|e| format!("GitHub's token: {e}"))
    }

    fn get(&self, url: &str) -> reqwest::RequestBuilder {
        self.http
            .get(url)
            .header("accept", "application/vnd.github+json")
            .header("x-github-api-version", "2022-11-28")
    }
}

/// A new connection's nonce, for its cookie.
pub fn nonce() -> String {
    let mut n = [0u8; 32];
    rand::rng().fill_bytes(&mut n);
    URL_SAFE_NO_PAD.encode(n)
}

/// The audience of the `state` (one of control's tokens, its `sub` the user) of the connection
/// whose cookie has `nonce`.
pub fn link_audience(nonce: &str) -> String {
    format!(
        "github-link:{}",
        URL_SAFE_NO_PAD.encode(Sha256::digest(nonce.as_bytes()))
    )
}

/// The `Set-Cookie` holding `nonce` for the ten minutes a connection may take.
pub fn set_cookie(nonce: &str) -> String {
    format!("{COOKIE}={nonce}; HttpOnly; Secure; SameSite=Lax; Max-Age=600")
}

pub fn clear_cookie() -> String {
    format!("{COOKIE}=; HttpOnly; Secure; SameSite=Lax; Max-Age=0")
}

/// The nonce a `Cookie` header holds.
pub fn from_cookies(header: &str) -> Option<&str> {
    header
        .split(';')
        .filter_map(|c| c.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE)
        .map(|(_, v)| v)
        .filter(|v| !v.is_empty())
}

/// What the App says it is with, to GitHub: a JWT of its key, good for nine minutes (GitHub
/// takes ten at most), from a minute ago (for clocks a little apart).
fn app_jwt(app: &App) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("after 1970")
        .as_secs();
    let claims = serde_json::json!({ "iss": app.client_id, "iat": now - 60, "exp": now + 540 });
    jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &app.key)
        .expect("an RSA key signs")
}

/// A GitHub for the tests: an App's, whose installations are those `fake::INSTALLATIONS` say.
#[cfg(test)]
pub mod fake {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use axum::extract::{Path, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::routing::{get, post};
    use axum::{Form, Json, Router};

    use super::GithubArgs;

    /// An RSA key for the fake App (2048 bits, PKCS#1, as GitHub gives them).
    pub const KEY: &str = include_str!("../tests/github-app-key.pem");

    pub struct GitHub {
        pub url: String,
        /// How many tokens were minted.
        pub minted: Arc<AtomicUsize>,
    }

    impl GitHub {
        /// `args` for it, the App's key written in `dir`.
        pub fn args(&self, dir: &std::path::Path) -> GithubArgs {
            let key = dir.join("github-app.pem");
            std::fs::write(&key, KEY).unwrap();
            GithubArgs {
                github_api: self.url.clone(),
                github_web: self.url.clone(),
                github_app_slug: Some("dirigent-test".into()),
                github_app_client_id: Some("Iv1.test".into()),
                github_app_client_secret: Some("client-secret".into()),
                github_app_key: Some(key),
            }
        }

        /// The code `code-<login>` is for the GitHub user `<login>`, who reaches the App's
        /// installation 7 on `<login>`'s account (and `user-two` also 8, on `org`'s). Of the
        /// repositories, `<x>/public` is public, `<x>/private` readable with `ghp_good`; the
        /// installations include `private` and not `other`.
        pub async fn start() -> GitHub {
            let minted = Arc::new(AtomicUsize::new(0));
            async fn exchange(Form(form): Form<Vec<(String, String)>>) -> Json<serde_json::Value> {
                let get = |k: &str| form.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
                if get("client_secret") != Some("client-secret") {
                    return Json(serde_json::json!({ "error": "incorrect_client_credentials" }));
                }
                match get("code").and_then(|c| c.strip_prefix("code-")) {
                    Some(login) => {
                        Json(serde_json::json!({ "access_token": format!("ghu_{login}") }))
                    }
                    None => Json(serde_json::json!({
                        "error": "bad_verification_code",
                        "error_description": "The code passed is incorrect or expired.",
                    })),
                }
            }
            async fn installations(headers: HeaderMap) -> (StatusCode, Json<serde_json::Value>) {
                let auth = headers.get("authorization").and_then(|a| a.to_str().ok());
                let Some(login) = auth.and_then(|a| a.strip_prefix("Bearer ghu_")) else {
                    return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({})));
                };
                let mut found = vec![serde_json::json!({ "id": 7, "account": { "login": login } })];
                if login == "user-two" {
                    found.push(serde_json::json!({ "id": 8, "account": { "login": "org" } }));
                }
                (
                    StatusCode::OK,
                    Json(serde_json::json!({ "installations": found })),
                )
            }
            async fn mint(
                State(minted): State<Arc<AtomicUsize>>,
                Path(id): Path<i64>,
                headers: HeaderMap,
                Json(body): Json<serde_json::Value>,
            ) -> (StatusCode, Json<serde_json::Value>) {
                let auth = headers.get("authorization").and_then(|a| a.to_str().ok());
                // an App's JWT: three parts
                if auth
                    .and_then(|a| a.strip_prefix("Bearer "))
                    .map(|t| t.split('.').count())
                    != Some(3)
                {
                    return (StatusCode::UNAUTHORIZED, Json(serde_json::json!({})));
                }
                if body["repositories"] != serde_json::json!(["private"]) {
                    return (
                        StatusCode::UNPROCESSABLE_ENTITY,
                        Json(serde_json::json!({})),
                    );
                }
                let n = minted.fetch_add(1, Ordering::SeqCst);
                let token = format!("ghs_installation{id}_{n}");
                (
                    StatusCode::CREATED,
                    Json(serde_json::json!({ "token": token })),
                )
            }
            async fn repo(
                Path((_, name)): Path<(String, String)>,
                headers: HeaderMap,
            ) -> StatusCode {
                let auth = headers.get("authorization").and_then(|a| a.to_str().ok());
                match (name.as_str(), auth) {
                    ("public", _) => StatusCode::OK,
                    ("private", Some("Bearer ghp_good")) => StatusCode::OK,
                    ("private", Some(a)) if a.starts_with("Bearer ghs_") => StatusCode::OK,
                    _ => StatusCode::NOT_FOUND,
                }
            }
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let app = Router::new()
                .route("/login/oauth/access_token", post(exchange))
                .route("/user/installations", get(installations))
                .route("/app/installations/{id}/access_tokens", post(mint))
                .route("/repos/{owner}/{name}", get(repo))
                .with_state(minted.clone());
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            GitHub { url, minted }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_github_url_is_its_repository_and_others_are_none() {
        let repo = Some(Repo {
            owner: "adamchol".into(),
            name: "my.repo".into(),
        });
        for url in [
            "https://github.com/adamchol/my.repo",
            "https://github.com/adamchol/my.repo.git",
            "https://github.com/adamchol/my.repo/",
            "git@github.com:adamchol/my.repo.git",
            "ssh://git@github.com/adamchol/my.repo",
        ] {
            assert_eq!(Repo::parse(url), repo, "{url}");
        }
        for url in [
            "https://gitlab.com/adamchol/repo",
            "https://github.com/adamchol",
            "https://github.com/adamchol/repo/tree/main",
            "https://github.com.evil.com/a/b",
            "file:///tmp/repo.git",
        ] {
            assert_eq!(Repo::parse(url), None, "{url}");
        }
    }

    #[test]
    fn an_app_is_all_of_its_arguments_or_none() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("key.pem");
        std::fs::write(&key, fake::KEY).unwrap();
        let mut args = GithubArgs {
            github_api: "https://api.github.com".into(),
            github_web: "https://github.com".into(),
            github_app_slug: None,
            github_app_client_id: None,
            github_app_client_secret: None,
            github_app_key: None,
        };
        let none = Github::new(&args).unwrap();
        assert!(!none.has_app() && none.connect_url("s", false).is_none());
        args.github_app_slug = Some("dirigent".into());
        assert!(Github::new(&args).is_err());
        args.github_app_client_id = Some("Iv1.abc".into());
        args.github_app_client_secret = Some("shh".into());
        args.github_app_key = Some(key);
        let github = Github::new(&args).unwrap();
        assert_eq!(
            github.connect_url("a.b", false).unwrap(),
            "https://github.com/apps/dirigent/installations/new?state=a.b"
        );
        assert_eq!(
            github.connect_url("a.b", true).unwrap(),
            "https://github.com/login/oauth/authorize?client_id=Iv1.abc&state=a.b"
        );
        // its JWT is RS256, by its client id, for under ten minutes
        let jwt = app_jwt(github.app.as_ref().unwrap());
        let header = jsonwebtoken::decode_header(&jwt).unwrap();
        assert_eq!(header.alg, Algorithm::RS256);
        let claims: serde_json::Value = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(jwt.split('.').nth(1).unwrap())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(claims["iss"], "Iv1.abc");
        let life = claims["exp"].as_u64().unwrap() - claims["iat"].as_u64().unwrap();
        assert!(life <= 600, "{life}");
    }
}
