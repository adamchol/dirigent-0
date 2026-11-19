//! Keys, tokens and private keys in imported tool output are replaced by `[redacted]`.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use regex::Regex;

/// The patterns, by what they find.
static PATTERNS: LazyLock<Vec<(&str, Regex)>> = LazyLock::new(|| {
    [
        ("Anthropic API key", r"sk-ant-[A-Za-z0-9_-]{20,}"),
        (
            "OpenAI API key",
            r"\bsk-(?:(?:proj|svcacct|admin)-[A-Za-z0-9_-]{40,}|[A-Za-z0-9]{40,})",
        ),
        (
            "GitHub token",
            r"\b(?:gh[pousr]_[A-Za-z0-9]{36,}|github_pat_[A-Za-z0-9_]{40,})",
        ),
        ("GitLab token", r"\bglpat-[A-Za-z0-9_-]{20,}"),
        ("AWS access key", r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b"),
        (
            "AWS secret key",
            r#"(?i)aws_secret_access_key["']?\s*[:=]\s*["']?([A-Za-z0-9/+]{40})"#,
        ),
        ("Slack token", r"\bxox[abprs]-[A-Za-z0-9-]{10,}"),
        ("Google API key", r"\bAIza[0-9A-Za-z_-]{35}"),
        ("Stripe key", r"\b[rs]k_live_[0-9A-Za-z]{20,}"),
        (
            "private key",
            r"-----BEGIN [A-Z ]*PRIVATE KEY-----[\s\S]*?-----END [A-Z ]*PRIVATE KEY-----",
        ),
    ]
    .into_iter()
    .map(|(what, re)| (what, Regex::new(re).expect("a valid pattern")))
    .collect()
});

/// How many secrets of each kind were redacted.
#[derive(Default)]
pub struct Found {
    counts: BTreeMap<&'static str, usize>,
}

impl Found {
    /// `text` with its secrets replaced by `[redacted]`, counted.
    pub fn redact(&mut self, text: &mut String) {
        for (what, re) in PATTERNS.iter() {
            if !re.is_match(text) {
                continue;
            }
            let mut n = 0;
            let replaced = re.replace_all(text, |caps: &regex::Captures| {
                n += 1;
                match caps.get(1) {
                    // the value alone, after what names it
                    Some(value) => {
                        let whole = caps.get(0).expect("a match");
                        let before = &text[whole.start()..value.start()];
                        format!("{before}[redacted]")
                    }
                    None => "[redacted]".to_string(),
                }
            });
            *text = replaced.into_owned();
            *self.counts.entry(what).or_default() += n;
        }
    }

    pub fn counts(&self) -> Vec<(String, usize)> {
        self.counts
            .iter()
            .map(|(k, n)| (k.to_string(), *n))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_redacted_and_counted_and_the_rest_kept() {
        let key = format!("sk-ant-api03-{}", "a".repeat(40));
        let gh = format!("ghp_{}", "B".repeat(36));
        // made here, so this file has none to find
        let aws = format!("AKIA{}", "ABCDEFGHIJKLMNOP");
        let pem = |edge: &str| format!("-----{edge} OPENSSH {}-----", "PRIVATE KEY");
        let mut text = format!(
            "export ANTHROPIC_API_KEY={key}\ngit push https://x:{gh}@github.com/a/b\n\
             aws_secret_access_key = {}\n{aws}\nsk-not-a-key\n{}\nabc\n{}\n",
            "c".repeat(40),
            pem("BEGIN"),
            pem("END"),
        );
        let mut found = Found::default();
        found.redact(&mut text);
        assert_eq!(
            text,
            "export ANTHROPIC_API_KEY=[redacted]\ngit push https://x:[redacted]@github.com/a/b\n\
             aws_secret_access_key = [redacted]\n[redacted]\nsk-not-a-key\n[redacted]\n"
        );
        assert_eq!(
            found.counts(),
            [
                ("AWS access key".to_string(), 1),
                ("AWS secret key".to_string(), 1),
                ("Anthropic API key".to_string(), 1),
                ("GitHub token".to_string(), 1),
                ("private key".to_string(), 1),
            ]
        );
    }
}
