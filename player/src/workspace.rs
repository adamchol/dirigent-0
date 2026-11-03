//! The session's workspace: the directory the agent works in, made from the spec's source the
//! first time a player runs the session. The player only makes it (cloning, for a git source);
//! after that, what's in it is the agent's. Whether it was made is in the log
//! ([`Event::WorkspaceReady`]), so a player that finds it gone on a later start makes it again.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use dirigent_protocol::control::Source;
use dirigent_protocol::Event;
use tokio::process::Command;
use tracing::info;

use crate::session::Session;

const CLONE_TIMEOUT: Duration = Duration::from_secs(600);
/// How much of git's stderr a failure keeps.
const ERROR_TAIL: usize = 2000;

/// The session's log, once the workspace in `workdir` is made from `source`: with the event that
/// says so appended, if there's one. A workspace that can't be made is a `WorkspaceFailed` in the
/// log, and an error.
pub async fn open(
    session: &mut dyn Session,
    workdir: &Path,
    source: Option<&Source>,
) -> anyhow::Result<Vec<(String, Event)>> {
    let mut log = session.load().await?;
    match prepare(workdir, source, &log).await {
        Ok(None) => {}
        Ok(Some(ev)) => {
            info!(?ev, "workspace ready");
            let id = session.publish(&ev).await;
            log.push((id, ev));
        }
        Err(error) => {
            session
                .publish(&Event::WorkspaceFailed {
                    error: error.clone(),
                })
                .await;
            anyhow::bail!("the workspace couldn't be made: {error}");
        }
    }
    Ok(log)
}

/// Make the workspace in `workdir` (which exists) from `source`, as `log` (the session's log so
/// far) needs: the event to append, if any, or why it couldn't be made.
pub async fn prepare(
    workdir: &Path,
    source: Option<&Source>,
    log: &[(String, Event)],
) -> Result<Option<Event>, String> {
    let ready = log
        .iter()
        .any(|(_, e)| matches!(e, Event::WorkspaceReady { .. }));
    let empty = std::fs::read_dir(workdir)
        .map_err(|e| format!("reading {}: {e}", workdir.display()))?
        .next()
        .is_none();
    match (source, ready, empty) {
        (_, true, false) | (None, true, true) => Ok(None),
        (None, false, _) => Ok(Some(Event::WorkspaceReady {
            source: None,
            head: None,
            recreated: false,
        })),
        (Some(_), false, false) => Err(format!(
            "{} isn't empty, so the source can't be cloned into it",
            workdir.display()
        )),
        (Some(source @ Source::Git { url, reference }), recreated, true) => {
            clone(url, reference.as_deref(), workdir).await?;
            Ok(Some(Event::WorkspaceReady {
                source: Some(source.clone()),
                head: Some(head(workdir).await?),
                recreated,
            }))
        }
    }
}

/// A partial clone: all of the history, with blobs fetched as they're needed. Public
/// repositories only: git never asks for credentials.
async fn clone(url: &str, reference: Option<&str>, workdir: &Path) -> Result<(), String> {
    let mut cmd = git();
    cmd.args(["clone", "--quiet", "--filter=blob:none"]);
    if let Some(r) = reference {
        cmd.args(["--branch", r]);
    }
    cmd.arg("--").arg(url).arg(workdir);
    run(cmd, CLONE_TIMEOUT)
        .await
        .map(drop)
        .map_err(|e| format!("cloning {url}: {e}"))
}

async fn head(workdir: &Path) -> Result<String, String> {
    let mut cmd = git();
    cmd.arg("-C").arg(workdir).args(["rev-parse", "HEAD"]);
    run(cmd, Duration::from_secs(30))
        .await
        .map(|out| out.trim().to_string())
        .map_err(|e| format!("reading the cloned commit: {e}"))
}

fn git() -> Command {
    let mut cmd = Command::new("git");
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

/// Its stdout, or the tail of its stderr.
async fn run(mut cmd: Command, timeout: Duration) -> Result<String, String> {
    let out = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| format!("timed out after {}s", timeout.as_secs()))?
        .map_err(|e| format!("running git: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let err = err.trim();
    let start = err.len().saturating_sub(ERROR_TAIL);
    let start = (start..err.len())
        .find(|&i| err.is_char_boundary(i))
        .unwrap_or(err.len());
    Err(format!("{} ({})", &err[start..], out.status))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A bare repository with one commit on `main` and one on `dev`; its `file://` URL.
    pub(crate) fn repo(dir: &Path) -> String {
        let sh = |script: &str| {
            let out = std::process::Command::new("sh")
                .arg("-ec")
                .arg(script)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
        };
        sh("git init -q -b main work
            cd work
            git config user.email t@t && git config user.name t
            echo main > file && git add file && git commit -qm main
            git checkout -qb dev && echo dev > file && git commit -qam dev
            git checkout -q main
            git clone -q --bare . ../repo.git");
        format!("file://{}/repo.git", dir.display())
    }

    fn git_source(url: &str, reference: Option<&str>) -> Source {
        Source::Git {
            url: url.into(),
            reference: reference.map(Into::into),
        }
    }

    fn ready(recreated: bool) -> Vec<(String, Event)> {
        vec![(
            "e0".into(),
            Event::WorkspaceReady {
                source: None,
                head: None,
                recreated,
            },
        )]
    }

    #[tokio::test]
    async fn a_git_source_is_cloned_into_an_empty_workspace_once() {
        let tmp = tempfile::tempdir().unwrap();
        let url = repo(tmp.path());
        let work = tmp.path().join("ws");
        std::fs::create_dir(&work).unwrap();
        let source = git_source(&url, None);
        let Some(Event::WorkspaceReady {
            source: Some(got),
            head: Some(head),
            recreated: false,
        }) = prepare(&work, Some(&source), &[]).await.unwrap()
        else {
            panic!("not ready");
        };
        assert_eq!(got, source);
        assert_eq!(head.len(), 40);
        assert_eq!(
            std::fs::read_to_string(work.join("file")).unwrap(),
            "main\n"
        );
        // a later start finds it there
        assert_eq!(prepare(&work, Some(&source), &ready(false)).await, Ok(None));
    }

    #[tokio::test]
    async fn a_ref_is_checked_out() {
        let tmp = tempfile::tempdir().unwrap();
        let url = repo(tmp.path());
        let work = tmp.path().join("ws");
        std::fs::create_dir(&work).unwrap();
        prepare(&work, Some(&git_source(&url, Some("dev"))), &[])
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(work.join("file")).unwrap(), "dev\n");
    }

    #[tokio::test]
    async fn a_workspace_found_gone_is_cloned_again() {
        let tmp = tempfile::tempdir().unwrap();
        let url = repo(tmp.path());
        let work = tmp.path().join("ws");
        std::fs::create_dir(&work).unwrap();
        let got = prepare(&work, Some(&git_source(&url, None)), &ready(false)).await;
        assert!(
            matches!(
                got,
                Ok(Some(Event::WorkspaceReady {
                    recreated: true,
                    ..
                }))
            ),
            "{got:?}"
        );
        assert!(work.join("file").exists());
    }

    #[tokio::test]
    async fn without_a_source_the_workspace_is_whatever_is_there() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("mine"), "x").unwrap();
        assert_eq!(
            prepare(tmp.path(), None, &[]).await,
            Ok(Some(Event::WorkspaceReady {
                source: None,
                head: None,
                recreated: false
            }))
        );
        assert_eq!(prepare(tmp.path(), None, &ready(false)).await, Ok(None));
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(prepare(empty.path(), None, &ready(false)).await, Ok(None));
    }

    #[tokio::test]
    async fn a_source_is_never_cloned_over_files() {
        let tmp = tempfile::tempdir().unwrap();
        let url = repo(tmp.path());
        let work = tmp.path().join("ws");
        std::fs::create_dir(&work).unwrap();
        std::fs::write(work.join("mine"), "x").unwrap();
        let err = prepare(&work, Some(&git_source(&url, None)), &[])
            .await
            .unwrap_err();
        assert!(err.contains("isn't empty"), "{err}");
    }

    #[tokio::test]
    async fn a_bad_url_fails_without_asking_for_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("file://{}/nothing.git", tmp.path().display());
        let err = prepare(tmp.path(), Some(&git_source(&url, None)), &[])
            .await
            .unwrap_err();
        assert!(err.starts_with("cloning file://"), "{err}");
    }
}
