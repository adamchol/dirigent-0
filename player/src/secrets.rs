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
