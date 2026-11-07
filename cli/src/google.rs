//! Signing in with Google as an installed app: the browser, PKCE, and the loopback for its
//! redirect; the ID token it gets is what control takes (its `aud` the CLI's client id, one of
//! control's `--oidc-audience`). A desktop client's secret isn't a secret (Google says as much):
//! control hands it out, with the client id.

use std::time::Duration;

use anyhow::Context;
use reqwest::Url;
use serde::Deserialize;

use crate::loopback::{self, Loopback};
use crate::oauth;

struct Endpoints {
    auth: String,
    token: String,
    revoke: String,
}

/// Google's, unless the tests point the CLI elsewhere.
fn endpoints() -> Endpoints {
    let var = |name: &str, default: &str| std::env::var(name).unwrap_or_else(|_| default.into());
    Endpoints {
        auth: var(
            "DIRIGENT_GOOGLE_AUTH_URL",
            "https://accounts.google.com/o/oauth2/v2/auth",
        ),
        token: var(
            "DIRIGENT_GOOGLE_TOKEN_URL",
            "https://oauth2.googleapis.com/token",
        ),
        revoke: var(
            "DIRIGENT_GOOGLE_REVOKE_URL",
            "https://oauth2.googleapis.com/revoke",
        ),
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: Option<String>,
    refresh_token: Option<String>,
}

/// An ID token, and the refresh token it's renewed with.
pub async fn sign_in(
    http: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    browser: bool,
) -> anyhow::Result<(String, Option<String>)> {
    let ep = endpoints();
    let lb = Loopback::bind().await?;
    let redirect = lb.redirect_uri();
    let state = oauth::random();
    let verifier = oauth::random();
    let url = Url::parse_with_params(
        &ep.auth,
        [
            ("client_id", client_id),
            ("redirect_uri", &redirect),
            ("response_type", "code"),
            ("scope", "openid email"),
            ("code_challenge", &oauth::challenge(&verifier)),
            ("code_challenge_method", "S256"),
            ("state", &state),
            // a refresh token, each time: signing in again is when an old one is replaced
            ("access_type", "offline"),
            ("prompt", "consent"),
        ],
    )?;
    loopback::open(url.as_str(), browser);
    let query = lb
        .wait(
            &state,
            Duration::from_secs(300),
            "Signed in to dirigent. You can close this tab.",
        )
        .await?;
    if let Some(error) = query.get("error") {
        anyhow::bail!("Google didn't sign you in: {error}");
    }
    let code = query.get("code").context("Google sent no code")?;
    let tokens: TokenResponse = token(
        http,
        &ep.token,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", &verifier),
            ("redirect_uri", &redirect),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ],
    )
    .await?;
    let id_token = tokens.id_token.context("Google sent no ID token")?;
    Ok((id_token, tokens.refresh_token))
}

/// A new ID token.
pub async fn refresh(
    http: &reqwest::Client,
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> anyhow::Result<String> {
    let tokens: TokenResponse = token(
        http,
        &endpoints().token,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
            ("client_secret", client_secret),
        ],
    )
    .await
    .context("renewing the Google sign-in (sign in again: dirigent login)")?;
    tokens.id_token.context("Google sent no ID token")
}

/// Ends the refresh token's grant; a token that's already invalid is no failure.
pub async fn revoke(http: &reqwest::Client, refresh_token: &str) -> anyhow::Result<()> {
    let res = http
        .post(endpoints().revoke)
        .form(&[("token", refresh_token)])
        .send()
        .await
        .context("revoking the Google sign-in")?;
    match res.status().as_u16() {
        200 | 400 => Ok(()),
        s => anyhow::bail!(
            "revoking the Google sign-in: {s} {}",
            res.text().await.unwrap_or_default()
        ),
    }
}

async fn token(
    http: &reqwest::Client,
    url: &str,
    form: &[(&str, &str)],
) -> anyhow::Result<TokenResponse> {
    let res = http.post(url).form(form).send().await?;
    let status = res.status();
    if !status.is_success() {
        anyhow::bail!(
            "Google's token endpoint: {status} {}",
            res.text().await.unwrap_or_default()
        );
    }
    Ok(res.json().await?)
}
