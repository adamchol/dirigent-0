//! Control's HTTP API as the signed-in caller: in `declared` auth, the subject it says; signed in
//! with Google, its ID token, renewed (and saved) when it's about to expire.

use std::path::PathBuf;

use anyhow::Context;
use reqwest::{Method, RequestBuilder};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::credentials::{self, Auth, Credentials};
use crate::{google, oauth};

/// The header control's `declared` auth takes the caller from.
const CALLER: &str = "dirigent-user";

pub struct Api {
    pub http: reqwest::Client,
    path: PathBuf,
    pub creds: Credentials,
}

impl Api {
    pub fn signed_in(http: reqwest::Client) -> anyhow::Result<Api> {
        let path = credentials::path()?;
        let creds =
            credentials::load(&path)?.context("not signed in: dirigent login <dirigent's URL>")?;
        Ok(Api { http, path, creds })
    }

    async fn request(&mut self, method: Method, path: &str) -> anyhow::Result<RequestBuilder> {
        let req = self
            .http
            .request(method, format!("{}{path}", self.creds.api));
        Ok(match &self.creds.auth {
            Auth::Declared { subject } => req.header(CALLER, subject.as_str()),
            Auth::Oidc { .. } => req.bearer_auth(self.id_token().await?),
        })
    }

    /// The Google ID token, renewed (and saved) when it's about to expire.
    async fn id_token(&mut self) -> anyhow::Result<String> {
        let Auth::Oidc {
            client_id,
            client_secret,
            id_token,
            refresh_token,
        } = &mut self.creds.auth
        else {
            anyhow::bail!("not signed in with Google");
        };
        if oauth::expiring(id_token, 300) {
            let refresh = refresh_token
                .as_deref()
                .context("the Google sign-in expired (sign in again: dirigent login)")?;
            *id_token = google::refresh(&self.http, client_id, client_secret, refresh).await?;
            let token = id_token.clone();
            credentials::save(&self.path, &self.creds)?;
            return Ok(token);
        }
        Ok(id_token.clone())
    }

    /// A credential for chlive: signed in with Google, the ID token; in `declared` auth, one
    /// control signs.
    pub async fn chlive_token(&mut self) -> anyhow::Result<String> {
        #[derive(serde::Deserialize)]
        struct Token {
            token: String,
        }
        match &self.creds.auth {
            Auth::Declared { .. } => Ok(self.post::<Token>("/token", &()).await?.token),
            Auth::Oidc { .. } => self.id_token().await,
        }
    }

    pub async fn get<T: DeserializeOwned>(&mut self, path: &str) -> anyhow::Result<T> {
        let req = self.request(Method::GET, path).await?;
        json(req).await
    }

    pub async fn post<T: DeserializeOwned>(
        &mut self,
        path: &str,
        body: &impl Serialize,
    ) -> anyhow::Result<T> {
        let req = self.request(Method::POST, path).await?.json(body);
        json(req).await
    }

    pub async fn delete(&mut self, path: &str) -> anyhow::Result<()> {
        let req = self.request(Method::DELETE, path).await?;
        checked(req).await.map(|_| ())
    }
}

async fn checked(req: RequestBuilder) -> anyhow::Result<reqwest::Response> {
    let res = req.send().await.context("reaching dirigent")?;
    let status = res.status();
    if !status.is_success() {
        let body = res.text().await.unwrap_or_default();
        anyhow::bail!("dirigent answered {status}: {body}");
    }
    Ok(res)
}

async fn json<T: DeserializeOwned>(req: RequestBuilder) -> anyhow::Result<T> {
    Ok(checked(req).await?.json().await?)
}
