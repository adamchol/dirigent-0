//! The session, handed to the VM's system while the player holds it (`--session-hook`, which
//! the players' VM sets; hosts' players never do): a directory with what it is (`env`) and its
//! secrets (`secrets/<NAME>`, kept as they change), the VM's hostname, and the systemd target
//! `dirigent-session.target`, which the cluster's own configuration hangs units on (a VPN, a
//! service seeded per session). Started once the lease is held; stopped, and the directory
//! removed, when the session ends.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use dirigent_protocol::control::{Part, SessionSpec, Source};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::secrets::Secrets;

const TARGET: &str = "dirigent-session.target";
/// How long the target's units may take to stop (a `tailscale logout`) before the VM goes on.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Hook {
    dir: PathBuf,
    /// Keeps `secrets/` as the session's secrets are.
    follow: JoinHandle<()>,
}

impl Hook {
    /// Hand the session `id`, which runs `spec` with `secrets`, to the system in `dir`.
    pub async fn start(dir: &Path, id: &str, spec: &SessionSpec, mut secrets: Secrets) -> Hook {
        let mut seen = secrets.current();
        if let Err(e) = write(dir, &env(id, spec), &seen) {
            warn!(
                "writing the session for the VM's system in {}: {e:#}",
                dir.display()
            );
        }
        let host = hostname(id);
        if let Err(e) = set_hostname(&host) {
            warn!("naming the VM {host}: {e}");
        }
        systemctl(&["start", "--no-block", TARGET]).await;
        info!(dir = %dir.display(), host, "the session is the VM's system's");
        let secrets_dir = dir.join("secrets");
        let follow = tokio::spawn(async move {
            while let Some(now) = secrets.changed(&seen).await {
                if let Err(e) = write_secrets(&secrets_dir, &now) {
                    warn!("updating the session's secrets for the VM's system: {e:#}");
                }
                seen = now;
            }
        });
        Hook {
            dir: dir.to_path_buf(),
            follow,
        }
    }

    /// The session ended: the target's units stop (within [`STOP_TIMEOUT`]), and its files go.
    pub async fn end(self) {
        self.follow.abort();
        match tokio::time::timeout(STOP_TIMEOUT, systemctl(&["stop", TARGET])).await {
            Ok(()) => {}
            Err(_) => warn!("{TARGET}'s units didn't stop within {STOP_TIMEOUT:?}"),
        }
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            warn!("removing {}: {e}", self.dir.display());
        }
    }
}

/// The VM's name while it runs session `id`: `dirigent-` and its first 8 characters.
fn hostname(id: &str) -> String {
    format!("dirigent-{}", id.chars().take(8).collect::<String>())
}

/// `env`'s variables: what the session is, for a script to source or a unit's `EnvironmentFile`.
fn env(id: &str, spec: &SessionSpec) -> Vec<(&'static str, String)> {
    let mut vars = vec![
        ("DIRIGENT_SESSION", id.to_string()),
        ("DIRIGENT_HOSTNAME", hostname(id)),
        (
            "DIRIGENT_PART",
            match spec.part {
                Part::Coder => "coder",
                Part::Dirigent => "dirigent",
            }
            .to_string(),
        ),
    ];
    if let Some(Source::Git { url, .. }) = &spec.source {
        vars.push(("DIRIGENT_REPOSITORY", url.clone()));
    }
    vars
}

/// The lines of `env`: single-quoted, which both a shell and systemd read. A value that can't be
/// (a quote or a line break in it) is left out.
fn env_text(vars: &[(&str, String)]) -> String {
    vars.iter()
        .filter(|(_, v)| !v.contains(['\'', '\n', '\r']))
        .map(|(k, v)| format!("{k}='{v}'\n"))
        .collect()
}

/// `dir` with `env` and `secrets/`, only root's.
fn write(
    dir: &Path,
    vars: &[(&str, String)],
    secrets: &BTreeMap<String, String>,
) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    replace(&dir.join("env"), env_text(vars).as_bytes())?;
    write_secrets(&dir.join("secrets"), secrets)
}

/// `dir` holds `secrets` and nothing else: a file each, named as the secret.
fn write_secrets(dir: &Path, secrets: &BTreeMap<String, String>) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    for (name, value) in secrets.iter().filter(|(n, _)| is_name(n)) {
        replace(&dir.join(name), value.as_bytes())?;
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let gone = name.to_str().is_none_or(|n| !secrets.contains_key(n));
        if gone {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

/// `path` with `content`, at once: written beside it (0600), then moved over it.
fn replace(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!(".{name}.tmp"));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(content)?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// An environment variable's name, as control allows secrets' names: never a path.
fn is_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn set_hostname(name: &str) -> std::io::Result<()> {
    // SAFETY: a pointer and length of a live buffer, which the kernel only reads
    let r = unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) };
    if r == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// `systemctl args`; what goes wrong is the player's log's, never the session's.
async fn systemctl(args: &[&str]) {
    let out = tokio::process::Command::new("systemctl")
        .args(args)
        .kill_on_drop(true)
        .output()
        .await;
    match out {
        Ok(o) if o.status.success() => {}
        Ok(o) => warn!(
            "systemctl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => warn!("systemctl {}: {e}", args.join(" ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn spec(repo: Option<&str>) -> SessionSpec {
        SessionSpec {
            source: repo.map(|url| Source::Git {
                url: url.into(),
                reference: None,
            }),
            ..SessionSpec::default()
        }
    }

    #[test]
    fn env_says_the_session_and_its_name_quoted() {
        let id = "3f2a1b2c-0000-4000-8000-000000000000";
        let text = env_text(&env(id, &spec(Some("https://github.com/you/app"))));
        assert_eq!(
            text,
            format!(
                "DIRIGENT_SESSION='{id}'\nDIRIGENT_HOSTNAME='dirigent-3f2a1b2c'\n\
                 DIRIGENT_PART='coder'\nDIRIGENT_REPOSITORY='https://github.com/you/app'\n"
            )
        );
        assert!(!env_text(&env(id, &spec(None))).contains("REPOSITORY"));
        // what can't be quoted is left out
        let odd = env_text(&env(id, &spec(Some("https://x/it's"))));
        assert!(!odd.contains("REPOSITORY"));
    }

    #[test]
    fn secrets_are_files_only_root_reads_and_follow_their_changes() {
        let dir = tempfile::tempdir().unwrap();
        let session = dir.path().join("session");
        let secrets = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let vars = env("s", &spec(None));
        write(
            &session,
            &vars,
            &secrets(&[("TS_AUTHKEY", "tskey-1"), ("NPM_TOKEN", "n")]),
        )
        .unwrap();
        let read = |name: &str| std::fs::read_to_string(session.join("secrets").join(name));
        assert_eq!(read("TS_AUTHKEY").unwrap(), "tskey-1");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&session), 0o700);
        assert_eq!(mode(&session.join("secrets")), 0o700);
        assert_eq!(mode(&session.join("secrets/TS_AUTHKEY")), 0o600);
        assert_eq!(mode(&session.join("env")), 0o600);

        // renewed, one gone, one new, one that isn't a name
        write_secrets(
            &session.join("secrets"),
            &secrets(&[("TS_AUTHKEY", "tskey-2"), ("GH", "g"), ("../x", "no")]),
        )
        .unwrap();
        assert_eq!(read("TS_AUTHKEY").unwrap(), "tskey-2");
        assert_eq!(read("GH").unwrap(), "g");
        assert!(read("NPM_TOKEN").is_err());
        let names: Vec<_> = std::fs::read_dir(session.join("secrets"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(!dir.path().join("x").exists());
    }
}
