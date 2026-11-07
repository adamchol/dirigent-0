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
        let req = match &mut self.creds.auth {
            Auth::Declared { subject } => req.header(CALLER, subject.as_str()),
            Auth::Oidc {
                client_id,
                client_secret,
                id_token,
                refresh_token,
            } => {
                if oauth::expiring(id_token, 300) {
                    let refresh = refresh_token
                        .as_deref()
                        .context("the Google sign-in expired (sign in again: dirigent login)")?;
                    *id_token =
                        google::refresh(&self.http, client_id, client_secret, refresh).await?;
                    credentials::save(&self.path, &self.creds)?;
                    let Auth::Oidc { id_token, .. } = &self.creds.auth else {
                        unreachable!()
                    };
                    req.bearer_auth(id_token)
                } else {
                    req.bearer_auth(id_token.as_str())
                }
            }
        };
        Ok(req)
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
