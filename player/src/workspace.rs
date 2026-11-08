//! The session's workspace: the directory the agent works in, made from the spec's source the
//! first time a player runs the session. The player only makes it (cloning, for a git source);
//! after that, what's in it is the agent's. Whether it was made is in the log
//! ([`Event::WorkspaceReady`]), so a player that finds it gone on a later start makes it again.

use std::path::Path;
use std::time::Duration;

use dirigent_protocol::control::Source;
use dirigent_protocol::Event;
use tokio::process::Command;
use tracing::{info, warn};

use crate::checkpoints::Checkpoints;
use crate::mirror::Mirror;
use crate::process::{command, run};
use crate::secrets::Secrets;
use crate::session::Session;

const CLONE_TIMEOUT: Duration = Duration::from_secs(600);

/// The session's log, once the workspace in `workdir` is made from `source`: with the event that
/// says so appended, if there's one; or why it can't be made (which the player reports as it
/// fails). And the workspace's mirror (its repository in `mirror`), made from the log and
/// brought up to date: a workspace restored from a checkpoint becomes the log's last commit, and
/// what's changed since is committed. Without git, the workspace isn't mirrored.
pub async fn open(
    session: &mut dyn Session,
    workdir: &'static Path,
    source: Option<&Source>,
    checkpoints: Option<&Checkpoints>,
    mirror: &Path,
    secrets: Secrets,
) -> anyhow::Result<(Vec<(String, Event)>, Option<Mirror>)> {
    let mut log = session.load().await?;
    let mut restored = false;
    match prepare(workdir, source, &log, checkpoints).await {
        Ok(None) => {}
        Ok(Some(ev)) => {
            info!(?ev, "workspace in place");
            restored = matches!(ev, Event::WorkspaceRestored { .. });
            let id = session.publish(&ev).await;
            log.push((id, ev));
        }
        Err(error) => anyhow::bail!("the workspace couldn't be made: {error}"),
    }
    let files = session.files().await?;
    let mut mirror = match Mirror::open(mirror, workdir, &files, secrets).await {
        Ok(m) => m,
        Err(e) => {
            warn!("the workspace isn't mirrored: {e:#}");
            return Ok((log, None));
        }
    };
    if restored {
        if let Err(e) = mirror.restore().await {
            warn!("the workspace stays as its checkpoint has it: {e:#}");
        }
    }
    for ev in mirror.snapshot("the player starts").await {
        session.publish(&ev).await;
    }
    Ok((log, Some(mirror)))
}

/// Make the workspace in `workdir` (which exists) from `source`, as `log` (the session's log so
/// far) needs: the event to append, if any, or why it couldn't be made. Found gone, it's
/// restored from the log's last checkpoint, given `checkpoints`; without, made again.
pub async fn prepare(
    workdir: &Path,
    source: Option<&Source>,
    log: &[(String, Event)],
    checkpoints: Option<&Checkpoints>,
) -> Result<Option<Event>, String> {
    let ready = log
        .iter()
        .any(|(_, e)| matches!(e, Event::WorkspaceReady { .. }));
    let empty = std::fs::read_dir(workdir)
        .map_err(|e| format!("reading {}: {e}", workdir.display()))?
        .next()
        .is_none();
    let last = log.iter().rev().find_map(|(_, e)| match e {
        Event::Checkpoint { snapshot, .. } => Some(snapshot),
        _ => None,
    });
    if let (true, Some(snapshot)) = (empty, last) {
        match checkpoints {
            Some(checkpoints) => {
                checkpoints.restore(snapshot, workdir).await?;
                return Ok(Some(Event::WorkspaceRestored {
                    snapshot: snapshot.clone(),
                }));
            }
            None => warn!(
                snapshot,
                "the workspace is gone, and checkpoints are off: making it again"
            ),
        }
    }
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
    let mut cmd = command("git");
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    cmd
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
        }) = prepare(&work, Some(&source), &[], None).await.unwrap()
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
        assert_eq!(
            prepare(&work, Some(&source), &ready(false), None).await,
            Ok(None)
        );
    }

    #[tokio::test]
    async fn a_ref_is_checked_out() {
        let tmp = tempfile::tempdir().unwrap();
        let url = repo(tmp.path());
        let work = tmp.path().join("ws");
        std::fs::create_dir(&work).unwrap();
        prepare(&work, Some(&git_source(&url, Some("dev"))), &[], None)
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
        let got = prepare(&work, Some(&git_source(&url, None)), &ready(false), None).await;
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
            prepare(tmp.path(), None, &[], None).await,
            Ok(Some(Event::WorkspaceReady {
                source: None,
                head: None,
                recreated: false
            }))
        );
        assert_eq!(
            prepare(tmp.path(), None, &ready(false), None).await,
            Ok(None)
        );
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            prepare(empty.path(), None, &ready(false), None).await,
            Ok(None)
        );
    }

    #[tokio::test]
    async fn a_source_is_never_cloned_over_files() {
        let tmp = tempfile::tempdir().unwrap();
        let url = repo(tmp.path());
        let work = tmp.path().join("ws");
        std::fs::create_dir(&work).unwrap();
        std::fs::write(work.join("mine"), "x").unwrap();
        let err = prepare(&work, Some(&git_source(&url, None)), &[], None)
            .await
            .unwrap_err();
        assert!(err.contains("isn't empty"), "{err}");
    }

    #[tokio::test]
    async fn a_bad_url_fails_without_asking_for_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("file://{}/nothing.git", tmp.path().display());
        let err = prepare(tmp.path(), Some(&git_source(&url, None)), &[], None)
            .await
            .unwrap_err();
        assert!(err.starts_with("cloning file://"), "{err}");
    }

    fn checkpointed(snapshot: &str) -> Vec<(String, Event)> {
        let mut log = ready(false);
        log.push((
            "e1".into(),
            Event::Checkpoint {
                turn: "t".into(),
                snapshot: snapshot.into(),
            },
        ));
        log
    }

    #[tokio::test]
    async fn a_workspace_found_gone_is_restored_from_the_last_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let url = repo(tmp.path());
        let cp = crate::checkpoints::tests::checkpoints(tmp.path()).await;
        let old = tmp.path().join("old");
        std::fs::create_dir(&old).unwrap();
        std::fs::write(old.join("notes"), "mine").unwrap();
        let snapshot = cp.snapshot(&old, "t").await.unwrap();
        let source = git_source(&url, None);

        let work = tmp.path().join("ws");
        std::fs::create_dir(&work).unwrap();
        let log = checkpointed(&snapshot);
        assert_eq!(
            prepare(&work, Some(&source), &log, Some(&cp)).await,
            Ok(Some(Event::WorkspaceRestored {
                snapshot: snapshot.clone()
            }))
        );
        assert_eq!(std::fs::read_to_string(work.join("notes")).unwrap(), "mine");
        // there, it's kept as it is
        assert_eq!(
            prepare(&work, Some(&source), &log, Some(&cp)).await,
            Ok(None)
        );

        // without checkpoints, it's made again from the source
        let other = tmp.path().join("other");
        std::fs::create_dir(&other).unwrap();
        let got = prepare(&other, Some(&source), &log, None).await;
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
        assert!(other.join("file").exists() && !other.join("notes").exists());
    }

    /// A log, and the workspace's events, to open; what's published.
    struct Logged {
        log: Vec<(String, Event)>,
        files: Vec<Event>,
        published: Vec<Event>,
    }

    #[async_trait::async_trait]
    impl Session for Logged {
        async fn load(&mut self) -> anyhow::Result<Vec<(String, Event)>> {
            Ok(self.log.clone())
        }
        async fn files(&mut self) -> anyhow::Result<Vec<Event>> {
            Ok(self.files.clone())
        }
        async fn recv(&mut self) -> Option<(String, Event)> {
            None
        }
        async fn publish(&mut self, event: &Event) -> String {
            self.published.push(event.clone());
            format!("p{}", self.published.len())
        }
    }

    fn leak(path: &Path) -> &'static Path {
        Box::leak(path.canonicalize().unwrap().into_boxed_path())
    }

    #[tokio::test]
    async fn a_restored_workspace_gets_the_changes_after_its_checkpoint_from_the_log() {
        let tmp = tempfile::tempdir().unwrap();
        let cp = crate::checkpoints::tests::checkpoints(tmp.path()).await;
        // the first machine: the workspace is checkpointed after a turn, then changed by the
        // next turn's tools before the player stops
        let old = tmp.path().join("old");
        std::fs::create_dir(&old).unwrap();
        std::fs::write(old.join("keep"), "1").unwrap();
        std::fs::write(old.join("gone"), "1").unwrap();
        let mut first = Logged {
            log: ready(false),
            files: Vec::new(),
            published: Vec::new(),
        };
        let secrets = || Secrets::channel(&[("GITHUB_TOKEN", "ghp_secret123")]).1;
        let (_, mirror) = open(
            &mut first,
            leak(&old),
            None,
            Some(&cp),
            &tmp.path().join("s1"),
            secrets(),
        )
        .await
        .unwrap();
        let mut mirror = mirror.unwrap();
        let snapshot = cp.snapshot(&old, "t").await.unwrap();
        std::fs::write(old.join("keep"), "2").unwrap();
        std::fs::remove_file(old.join("gone")).unwrap();
        std::fs::write(old.join(".env"), "TOKEN=ghp_secret123\n").unwrap();
        let mut files = first.published.clone();
        files.extend(mirror.snapshot("Bash c1").await);
        assert_eq!(files.len(), 2, "{files:?}");

        // the next, with a workspace and a state of its own
        let work = tmp.path().join("ws");
        std::fs::create_dir(&work).unwrap();
        let mut next = Logged {
            log: checkpointed(&snapshot),
            files: files.clone(),
            published: Vec::new(),
        };
        let (_, mirror) = open(
            &mut next,
            leak(&work),
            None,
            Some(&cp),
            &tmp.path().join("s2"),
            secrets(),
        )
        .await
        .unwrap();
        assert_eq!(
            next.published,
            vec![Event::WorkspaceRestored { snapshot }],
            "restored, with nothing new to mirror"
        );
        assert_eq!(std::fs::read_to_string(work.join("keep")).unwrap(), "2");
        assert!(!work.join("gone").exists());
        // a secret it wrote: redacted in the log, put back in the workspace
        assert_eq!(
            std::fs::read_to_string(work.join(".env")).unwrap(),
            "TOKEN=ghp_secret123\n"
        );
        let reader = tempfile::tempdir().unwrap();
        let out = tempfile::tempdir().unwrap();
        let repo = dirigent_sync::Repo::open(reader.path()).await.unwrap();
        let mut bundles = dirigent_sync::Bundles::default();
        let whole: Vec<_> = files.iter().filter_map(|e| bundles.push(e)).collect();
        let (last, _) = repo.replay(whole).await;
        repo.checkout(out.path(), &last.unwrap(), true)
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(out.path().join(".env")).unwrap(),
            "TOKEN=[secret:GITHUB_TOKEN]\n"
        );

        // and its next commit builds on the log's last
        std::fs::write(work.join("new"), "n").unwrap();
        let more = mirror.unwrap().snapshot("Write c2").await;
        let (Event::FilesChanged { parent, .. }, Event::FilesChanged { commit, .. }) =
            (&more[0], &files[1])
        else {
            panic!("{more:?}");
        };
        assert_eq!(parent.as_ref(), Some(commit));
    }
}
