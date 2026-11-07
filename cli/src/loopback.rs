//! Where a browser comes back to after signing in: a one-shot HTTP listener on `127.0.0.1`, at
//! `/auth/callback` on a port of its own (OAuth for native apps, RFC 8252). Google's and OpenAI's
//! sign-ins both redirect there, with the authorization's result in the query.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context;
use reqwest::Url;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const PATH: &str = "/auth/callback";

pub struct Loopback {
    listener: TcpListener,
    pub port: u16,
}

impl Loopback {
    pub async fn bind() -> anyhow::Result<Loopback> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .context("listening on 127.0.0.1 for the sign-in to come back")?;
        let port = listener.local_addr()?.port();
        Ok(Loopback { listener, port })
    }

    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}{PATH}", self.port)
    }

    /// The callback's query, once a request for it with this `state` comes; the browser is told
    /// `done` (or that it failed). Anything else asked of the listener (a favicon) is answered
    /// 404, and a callback with another state is turned away: it isn't this sign-in's.
    pub async fn wait(
        self,
        state: &str,
        timeout: Duration,
        done: &str,
    ) -> anyhow::Result<HashMap<String, String>> {
        tokio::time::timeout(timeout, async {
            loop {
                let (mut conn, _) = self.listener.accept().await?;
                let Some(target) = read_target(&mut conn).await else {
                    continue;
                };
                let Some(query) = callback_query(&target) else {
                    respond(&mut conn, "404 Not Found", "Not found.").await;
                    continue;
                };
                if query.get("state").map(String::as_str) != Some(state) {
                    respond(
                        &mut conn,
                        "400 Bad Request",
                        "This isn't the sign-in dirigent started.",
                    )
                    .await;
                    continue;
                }
                match query.get("error") {
                    Some(error) => {
                        respond(
                            &mut conn,
                            "200 OK",
                            &format!("Not signed in: {error}. You can close this tab."),
                        )
                        .await
                    }
                    None => respond(&mut conn, "200 OK", done).await,
                }
                return Ok(query);
            }
        })
        .await
        .map_err(|_| {
            anyhow::anyhow!("the sign-in didn't come back within {}s", timeout.as_secs())
        })?
    }
}

/// The request line's target, of the first request on the connection.
async fn read_target(conn: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(10), conn.read(&mut chunk))
            .await
            .ok()?
            .ok()?;
        if n == 0 || buf.len() > 64 * 1024 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&buf);
    let line = head.lines().next()?;
    let mut parts = line.split(' ');
    match (parts.next(), parts.next()) {
        (Some("GET"), Some(target)) => Some(target.to_string()),
        _ => None,
    }
}

/// The query of a request for the callback's path; `None` for any other.
fn callback_query(target: &str) -> Option<HashMap<String, String>> {
    let url = Url::parse(&format!("http://127.0.0.1{target}")).ok()?;
    (url.path() == PATH).then(|| url.query_pairs().into_owned().collect())
}

async fn respond(conn: &mut TcpStream, status: &str, text: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>dirigent</title>\
         <body style=\"font:15px system-ui;margin:3rem\">{}</body>",
        html_escape(text)
    );
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = conn.write_all(response.as_bytes()).await;
    let _ = conn.shutdown().await;
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Opens `url` in the system's browser, if it can; it's printed either way, for when it can't.
pub fn open(url: &str, browser: bool) {
    eprintln!("Open this to sign in:\n\n  {url}\n");
    if !browser {
        return;
    }
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(program)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_callbacks_path_has_a_query_taken() {
        let q = callback_query("/auth/callback?code=c%2B1&state=s&client_id=oaiapp_1").unwrap();
        assert_eq!(q["code"], "c+1");
        assert_eq!(q["state"], "s");
        assert_eq!(q["client_id"], "oaiapp_1");
        assert!(callback_query("/favicon.ico").is_none());
        assert!(callback_query("/callback?code=c").is_none());
    }

    #[tokio::test]
    async fn the_callback_with_this_state_is_returned_and_others_turned_away() {
        let lb = Loopback::bind().await.unwrap();
        let base = format!("http://127.0.0.1:{}", lb.port);
        let wait =
            tokio::spawn(async move { lb.wait("s1", Duration::from_secs(10), "Signed in.").await });
        let http = reqwest::Client::new();
        let other = http
            .get(format!("{base}/favicon.ico"))
            .send()
            .await
            .unwrap();
        assert_eq!(other.status(), 404);
        let wrong = http
            .get(format!("{base}{PATH}?state=nope&code=x"))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), 400);
        let ok = http
            .get(format!("{base}{PATH}?state=s1&code=abc"))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200);
        assert!(ok.text().await.unwrap().contains("Signed in."));
        let q = wait.await.unwrap().unwrap();
        assert_eq!(q["code"], "abc");
    }
}
