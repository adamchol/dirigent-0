//! The session's secrets, as its lease has them (control sends them again at once when they
//! change): put in its tools' environment, and kept out of what the player publishes, each value
//! replaced by `[secret:NAME]`. Only in memory: never in the player's own environment, nor on disk.

use std::collections::BTreeMap;

use dirigent_protocol::control::pb;
use tokio::sync::watch;

/// Values shorter than this aren't redacted: a secret `true` would garble every output.
const MIN_REDACTED: usize = 6;

#[derive(Clone)]
pub struct Secrets {
    tokens: watch::Receiver<pb::Tokens>,
}

impl Secrets {
    /// The secrets the lease's credentials (`tokens`) carry.
    pub fn new(tokens: watch::Receiver<pb::Tokens>) -> Secrets {
        Secrets { tokens }
    }

    /// None, ever.
    #[cfg(test)]
    pub fn none() -> Secrets {
        Secrets::new(watch::channel(pb::Tokens::default()).1)
    }

    /// `secrets`, and what changes them.
    #[cfg(test)]
    pub fn channel(secrets: &[(&str, &str)]) -> (watch::Sender<pb::Tokens>, Secrets) {
        let (tx, rx) = watch::channel(tokens(secrets));
        (tx, Secrets::new(rx))
    }

    /// The secrets now, by name.
    pub fn current(&self) -> BTreeMap<String, String> {
        self.tokens.borrow().secrets.clone().into_iter().collect()
    }

    /// The secrets once they differ from `seen` (at once if they do); `None` once they never
    /// will. Cancel-safe: what changed while it wasn't polled is seen on the next call.
    pub async fn changed(
        &mut self,
        seen: &BTreeMap<String, String>,
    ) -> Option<BTreeMap<String, String>> {
        loop {
            let now = self.current();
            if now != *seen {
                return Some(now);
            }
            self.tokens.changed().await.ok()?;
        }
    }

    /// The environment of the processes tools start: the secrets, and, with a `GITHUB_TOKEN`
    /// among them, git's use of it ([`git_credentials`]).
    pub fn env(&self) -> Vec<(String, String)> {
        let secrets = self.current();
        let git = git_credentials(&secrets);
        secrets.into_iter().chain(git).collect()
    }

    /// `text` with every secret's value in it replaced by `[secret:NAME]`.
    pub fn redact(&self, text: &str) -> String {
        redact(text, &self.current())
    }
}

/// `text` with each of `secrets`' values replaced by `[secret:NAME]`, the longest first (so one
/// that contains another is replaced whole).
pub fn redact(text: &str, secrets: &BTreeMap<String, String>) -> String {
    let mut values: Vec<(&String, &String)> = secrets
        .iter()
        .filter(|(_, v)| v.len() >= MIN_REDACTED)
        .collect();
    values.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    let mut out = text.to_string();
    for (name, value) in values {
        if out.contains(value.as_str()) {
            out = out.replace(value.as_str(), &format!("[secret:{name}]"));
        }
    }
    out
}

/// The values to redact, as `dirigent_sync` takes them: `(name, value)`, too short ones left out.
pub fn redactions(secrets: &BTreeMap<String, String>) -> Vec<(String, String)> {
    secrets
        .iter()
        .filter(|(_, v)| v.len() >= MIN_REDACTED)
        .map(|(n, v)| (n.clone(), v.clone()))
        .collect()
}

/// The secret git uses for GitHub, and `gh` too.
pub const GITHUB_TOKEN: &str = "GITHUB_TOKEN";

/// With a `GITHUB_TOKEN` in `secrets`, the variables that make git use it for github.com: a
/// credential helper that reads it from the environment each time git asks (so it's never
/// written down, nor in a remote's URL), and `git@github.com:` URLs fetched over HTTPS with it.
/// Without, none.
pub fn git_credentials(secrets: &BTreeMap<String, String>) -> Vec<(String, String)> {
    if !secrets.contains_key(GITHUB_TOKEN) {
        return Vec::new();
    }
    let helper = format!(
        "!f() {{ test \"$1\" = get && echo username=x-access-token && echo \"password=${GITHUB_TOKEN}\"; }}; f"
    );
    [
        ("GIT_CONFIG_COUNT", "2"),
        ("GIT_CONFIG_KEY_0", "credential.https://github.com.helper"),
        ("GIT_CONFIG_VALUE_0", &helper),
        ("GIT_CONFIG_KEY_1", "url.https://github.com/.insteadOf"),
        ("GIT_CONFIG_VALUE_1", "git@github.com:"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

#[cfg(test)]
pub fn tokens(secrets: &[(&str, &str)]) -> pb::Tokens {
    pb::Tokens {
        secrets: secrets
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_are_redacted_by_name_the_longest_first_and_short_ones_kept() {
        let secrets: BTreeMap<String, String> = [
            ("TOKEN", "ghp_abcdef"),
            ("LONGER", "ghp_abcdef123"),
            ("SHORT", "abc"),
        ]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(
            redact("a ghp_abcdef123 b ghp_abcdef c abc", &secrets),
            "a [secret:LONGER] b [secret:TOKEN] c abc"
        );
        assert_eq!(redactions(&secrets).len(), 2);
    }

    #[test]
    fn git_asks_the_environment_for_a_github_token_and_nothing_else() {
        let (_, secrets) = Secrets::channel(&[("OTHER", "whatever")]);
        assert_eq!(secrets.env().len(), 1);
        let (_, secrets) = Secrets::channel(&[(GITHUB_TOKEN, "ghp_the_token")]);
        let fill = |host: &str| {
            let mut child = std::process::Command::new("git")
                .args(["credential", "fill"])
                .env_clear()
                .env("PATH", std::env::var_os("PATH").unwrap())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_TERMINAL_PROMPT", "0")
                .envs(secrets.env())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            use std::io::Write;
            let mut stdin = child.stdin.take().unwrap();
            write!(stdin, "protocol=https\nhost={host}\n\n").unwrap();
            drop(stdin);
            let out = child.wait_with_output().unwrap();
            (out.status.success(), String::from_utf8(out.stdout).unwrap())
        };
        let (ok, out) = fill("github.com");
        assert!(ok);
        assert!(
            out.contains("username=x-access-token\npassword=ghp_the_token\n"),
            "{out}"
        );
        // another host's credentials aren't given, so git can't ask without a terminal
        assert!(!fill("gitlab.com").0);
        // nor is the token in the configuration itself
        assert!(!secrets
            .env()
            .iter()
            .any(|(k, v)| k != GITHUB_TOKEN && v.contains("ghp_")));
    }

    #[tokio::test]
    async fn a_change_is_seen_only_when_the_secrets_differ() {
        let (tx, mut secrets) = Secrets::channel(&[("A", "aaaaaa")]);
        let seen = secrets.current();
        let watching = tokio::spawn(async move { secrets.changed(&seen).await });
        // sent again as it was: not a change
        tx.send_replace(tokens(&[("A", "aaaaaa")]));
        tokio::task::yield_now().await;
        tx.send_replace(tokens(&[("A", "aaaaaa"), ("B", "bbbbbb")]));
        let got = watching.await.unwrap().unwrap();
        assert_eq!(got.keys().collect::<Vec<_>>(), ["A", "B"]);
    }
}
