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
use crate::secrets::{git_credentials, Secrets, GITHUB_TOKEN};
use crate::session::Session;

const CLONE_TIMEOUT: Duration = Duration::from_secs(600);

/// The session's log, once the workspace in `workdir` is made from `source`: with the event that
/// says so appended, if there's one; or why it can't be made (which the player reports as it
/// fails). And the workspace's mirror (its repository in `mirror`), made from the log and
/// brought up to date: a workspace restored from a checkpoint, made here (the first time, as an
/// import's is, or again, found gone with no checkpoint to restore), or kept here while the
/// session went on elsewhere, becomes the log's last commit, and what's changed since is
/// committed. Without git, the workspace isn't mirrored.
pub async fn open(
    session: &mut dyn Session,
    workdir: &'static Path,
    source: Option<&Source>,
    checkpoints: Option<&Checkpoints>,
    mirror: &Path,
    secrets: Secrets,
) -> anyhow::Result<(Vec<(String, Event)>, Option<Mirror>)> {
    let mut log = session.load().await?;
    let empty = is_empty(workdir).map_err(|e| anyhow::anyhow!(e))?;
    let ready = log
        .iter()
        .any(|(_, e)| matches!(e, Event::WorkspaceReady { .. }));
    let mut made = match prepare(workdir, source, &log, checkpoints, &secrets).await {
        Ok(made) => made,
        Err(error) => anyhow::bail!("the workspace couldn't be made: {error}"),
    };
    let files = session.files().await?;
    let mirror = match Mirror::open(mirror, workdir, &files).await {
        Ok(m) => Some(m),
        Err(e) => {
            warn!("the workspace isn't mirrored: {e:#}");
            None
        }
    };
    // a workspace without a source, found gone: it has only what the log has
    let gone = made.is_none() && empty && ready;
    let restore = match &mirror {
        Some(m) if gone => m.has_files().await,
        // kept here while the session ran elsewhere (a host's), and changed there since
        Some(m) if made.is_none() => m.behind().await,
        _ => made.is_some(),
    };
    if let Some(m) = mirror.as_ref().filter(|_| restore) {
        match m.restore().await {
            Ok(Some(commit)) => match &mut made {
                Some(Event::WorkspaceReady { files, .. }) => *files = Some(commit),
                Some(_) => {}
                None if gone => {
                    made = Some(Event::WorkspaceReady {
                        source: None,
                        head: None,
                        recreated: true,
                        files: Some(commit),
                    })
                }
                // brought up to date where it was: the session's move is in the log already
                None => {}
            },
            Ok(None) => {}
            Err(e) => warn!("the workspace stays as it was made: {e:#}"),
        }
    }
    if let Some(ev) = made {
        info!(?ev, "workspace in place");
        let id = session.publish(&ev).await;
        log.push((id, ev));
    }
    let Some(mut mirror) = mirror else {
        return Ok((log, None));
    };
    for ev in mirror.snapshot("the player starts").await {
        session.publish(&ev).await;
    }
    Ok((log, Some(mirror)))
}

fn is_empty(dir: &Path) -> Result<bool, String> {
    Ok(std::fs::read_dir(dir)
        .map_err(|e| format!("reading {}: {e}", dir.display()))?
        .next()
        .is_none())
}

/// Make the workspace in `workdir` (which exists) from `source`, as `log` (the session's log so
/// far) needs: the event to append, if any, or why it couldn't be made. Found gone, it's
/// restored from the log's last checkpoint, given `checkpoints`; without, made again. A git
/// source is cloned with the session's GitHub token, if it has one (`secrets`).
pub async fn prepare(
    workdir: &Path,
    source: Option<&Source>,
    log: &[(String, Event)],
    checkpoints: Option<&Checkpoints>,
    secrets: &Secrets,
) -> Result<Option<Event>, String> {
    let ready = log
        .iter()
        .any(|(_, e)| matches!(e, Event::WorkspaceReady { .. }));
    let empty = is_empty(workdir)?;
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
            files: None,
        })),
        (Some(_), false, false) => Err(format!(
            "{} isn't empty, so the source can't be cloned into it",
            workdir.display()
        )),
        (Some(source @ Source::Git { url, reference }), recreated, true) => {
            clone(url, reference.as_deref(), workdir, secrets).await?;
            Ok(Some(Event::WorkspaceReady {
                source: Some(source.clone()),
                head: Some(head(workdir).await?),
                recreated,
                files: None,
            }))
        }
    }
}

/// A partial clone: all of the history, with blobs fetched as they're needed. On GitHub, with
/// the session's `GITHUB_TOKEN` if it has one; git never asks for credentials otherwise.
async fn clone(
    url: &str,
    reference: Option<&str>,
    workdir: &Path,
    secrets: &Secrets,
) -> Result<(), String> {
    let secrets = secrets.current();
    let mut cmd = git();
    cmd.envs(
        secrets
            .get_key_value(GITHUB_TOKEN)
            .map(|(k, v)| (k.clone(), v.clone())),
    )
    .envs(git_credentials(&secrets));
    cmd.args(["clone", "--quiet", "--filter=blob:none"]);
    if let Some(r) = reference {
        cmd.args(["--branch", r]);
    }
    cmd.arg("--").arg(url).arg(workdir);
    run(cmd, CLONE_TIMEOUT)
        .await
        .map(drop)
        .map_err(|e| clone_failed(url, secrets.contains_key(GITHUB_TOKEN), &e))
}

/// Why cloning `url` failed, from git's `error`: said plainly when it's GitHub refusing it, with
/// what gives access (`token`: the session has a `GITHUB_TOKEN`).
fn clone_failed(url: &str, token: bool, error: &str) -> String {
    let refused = [
        "could not read Username",
        "Repository not found",
        "Authentication failed",
    ]
    .iter()
    .any(|s| error.contains(s));
    match (refused, token) {
        (false, _) => format!("cloning {url}: {error}"),
        (true, false) => format!(
            "{url} is private or doesn't exist: the session has no GitHub access to it. Add a \
             GITHUB_TOKEN secret (a fine-grained token with Contents read and write on the \
             repository), or connect GitHub if dirigent's GitHub App is set up, then start the \
             session again"
        ),
        (true, true) => format!(
            "GitHub refused the session's GITHUB_TOKEN for {url}: it may have expired, or not \
             cover this repository (or the repository doesn't exist). Give the secret a token \
             that can read it, then start the session again"
        ),
    }
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
                files: None,
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
            ..
        }) = prepare(&work, Some(&source), &[], None, &Secrets::none())
            .await
            .unwrap()
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
            prepare(&work, Some(&source), &ready(false), None, &Secrets::none()).await,
            Ok(None)
        );
    }

    #[tokio::test]
    async fn a_ref_is_checked_out() {
        let tmp = tempfile::tempdir().unwrap();
        let url = repo(tmp.path());
        let work = tmp.path().join("ws");
        std::fs::create_dir(&work).unwrap();
        prepare(
            &work,
            Some(&git_source(&url, Some("dev"))),
            &[],
            None,
            &Secrets::none(),
        )
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
        let got = prepare(
            &work,
            Some(&git_source(&url, None)),
            &ready(false),
            None,
            &Secrets::none(),
        )
        .await;
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
            prepare(tmp.path(), None, &[], None, &Secrets::none()).await,
            Ok(Some(Event::WorkspaceReady {
                source: None,
                head: None,
                recreated: false,
                files: None,
            }))
        );
        assert_eq!(
            prepare(tmp.path(), None, &ready(false), None, &Secrets::none()).await,
            Ok(None)
        );
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(
            prepare(empty.path(), None, &ready(false), None, &Secrets::none()).await,
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
        let err = prepare(
            &work,
            Some(&git_source(&url, None)),
            &[],
            None,
            &Secrets::none(),
        )
        .await
        .unwrap_err();
        assert!(err.contains("isn't empty"), "{err}");
    }

    #[tokio::test]
    async fn a_bad_url_fails_without_asking_for_credentials() {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("file://{}/nothing.git", tmp.path().display());
        let err = prepare(
            tmp.path(),
            Some(&git_source(&url, None)),
            &[],
            None,
            &Secrets::none(),
        )
        .await
        .unwrap_err();
        assert!(err.starts_with("cloning file://"), "{err}");
    }

    #[test]
    fn github_refusing_a_clone_says_what_gives_access() {
        let url = "https://github.com/o/private";
        let prompt = "fatal: could not read Username for 'https://github.com': terminal prompts disabled (exit status: 128)";
        let without = clone_failed(url, false, prompt);
        assert!(
            without.starts_with("https://github.com/o/private is private"),
            "{without}"
        );
        assert!(without.contains("GITHUB_TOKEN secret"), "{without}");
        let refused = "remote: Repository not found.\nfatal: repository 'https://github.com/o/private/' not found";
        let with = clone_failed(url, true, refused);
        assert!(
            with.starts_with("GitHub refused the session's GITHUB_TOKEN"),
            "{with}"
        );
        // anything else is git's own words
        assert_eq!(
            clone_failed(url, true, "fatal: unable to access: timeout"),
            "cloning https://github.com/o/private: fatal: unable to access: timeout"
        );
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
            prepare(&work, Some(&source), &log, Some(&cp), &Secrets::none()).await,
            Ok(Some(Event::WorkspaceRestored {
                snapshot: snapshot.clone()
            }))
        );
        assert_eq!(std::fs::read_to_string(work.join("notes")).unwrap(), "mine");
        // there, it's kept as it is
        assert_eq!(
            prepare(&work, Some(&source), &log, Some(&cp), &Secrets::none()).await,
            Ok(None)
        );

        // without checkpoints, it's made again from the source
        let other = tmp.path().join("other");
        std::fs::create_dir(&other).unwrap();
        let got = prepare(&other, Some(&source), &log, None, &Secrets::none()).await;
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
        // too big for the log: it comes back from the checkpoint alone
        let big = std::fs::File::create(old.join("big")).unwrap();
        big.set_len(dirigent_sync::MAX_FILE_SIZE + 1).unwrap();
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
        assert_eq!(
            std::fs::metadata(work.join("big")).unwrap().len(),
            dirigent_sync::MAX_FILE_SIZE + 1
        );
        // The log restores the exact bytes written after the checkpoint.
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
            "TOKEN=ghp_secret123\n"
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

    #[tokio::test]
    async fn imported_secret_markers_stay_literal_and_git_clean_on_repeated_restore() {
        let tmp = tempfile::tempdir().unwrap();
        let url = repo(tmp.path());
        let local = tmp.path().join("work");
        let expected = "literal=[secret:TEST_TOKEN]\nvalue=test-secret-value\n";
        std::fs::write(local.join("file"), expected).unwrap();
        let out = std::process::Command::new("sh")
            .args([
                "-ec",
                "git commit -qam markers && git push ../repo.git main",
            ])
            .current_dir(&local)
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let source = git_source(&url, None);
        let repo = dirigent_sync::Repo::open(&tmp.path().join("cli"))
            .await
            .unwrap();
        let mut files = dirigent_sync::Workspace::new(&local);
        let snapshot = repo
            .snapshot(&mut files, None, "imported")
            .await
            .unwrap()
            .unwrap();
        let mut session = Logged {
            log: Vec::new(),
            files: dirigent_sync::events(&snapshot),
            published: Vec::new(),
        };
        let work = tmp.path().join("ws");
        std::fs::create_dir(&work).unwrap();
        let (_, mirror) = open(
            &mut session,
            leak(&work),
            Some(&source),
            None,
            &tmp.path().join("mirror"),
            Secrets::channel(&[("TEST_TOKEN", "test-secret-value")]).1,
        )
        .await
        .unwrap();
        let mut mirror = mirror.unwrap();
        for _ in 0..3 {
            assert_eq!(
                std::fs::read(work.join("file")).unwrap(),
                expected.as_bytes()
            );
            let status = std::process::Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&work)
                .output()
                .unwrap();
            assert!(status.status.success());
            assert!(status.stdout.is_empty(), "{status:?}");
            assert!(mirror.snapshot("unchanged").await.is_empty());
            mirror.restore().await.unwrap();
        }
    }

    #[tokio::test]
    async fn an_imported_workspace_is_its_source_with_the_files_from_the_log() {
        let tmp = tempfile::tempdir().unwrap();
        let url = repo(tmp.path());
        // the local working tree the import carried: a file changed, one deleted, one new
        let local = tmp.path().join("work");
        std::fs::write(local.join("file"), "changed\n").unwrap();
        std::fs::write(local.join("new"), "n").unwrap();
        let repo = dirigent_sync::Repo::open(&tmp.path().join("cli"))
            .await
            .unwrap();
        let mut files = dirigent_sync::Workspace::new(&local);
        let snapshot = repo
            .snapshot(&mut files, None, "imported")
            .await
            .unwrap()
            .unwrap();
        let files = dirigent_sync::events(&snapshot);

        let work = tmp.path().join("ws");
        std::fs::create_dir(&work).unwrap();
        let mut session = Logged {
            log: Vec::new(),
            files: files.clone(),
            published: Vec::new(),
        };
        let source = git_source(&url, None);
        open(
            &mut session,
            leak(&work),
            Some(&source),
            None,
            &tmp.path().join("mirror"),
            Secrets::none(),
        )
        .await
        .unwrap();
        assert!(
            matches!(&session.published[..], [Event::WorkspaceReady { .. }]),
            "cloned, and nothing new to mirror: {:?}",
            session.published
        );
        assert_eq!(
            std::fs::read_to_string(work.join("file")).unwrap(),
            "changed\n"
        );
        assert_eq!(std::fs::read_to_string(work.join("new")).unwrap(), "n");
        // on the source's history
        assert!(work.join(".git").is_dir());

        // a later start leaves it as the session made it
        std::fs::write(work.join("file"), "later\n").unwrap();
        let mut again = Logged {
            log: vec![("e0".into(), session.published[0].clone())],
            files,
            published: Vec::new(),
        };
        open(
            &mut again,
            leak(&work),
            Some(&source),
            None,
            &tmp.path().join("mirror2"),
            Secrets::none(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(work.join("file")).unwrap(),
            "later\n"
        );
    }

    #[tokio::test]
    async fn a_workspace_found_gone_without_checkpoints_gets_the_sessions_files_from_the_log() {
        let tmp = tempfile::tempdir().unwrap();
        let url = repo(tmp.path());
        // what the session had: its source's files, one changed and one new
        let had = tmp.path().join("had");
        std::fs::create_dir(&had).unwrap();
        std::fs::write(had.join("file"), "changed\n").unwrap();
        std::fs::write(had.join("new"), "n").unwrap();
        let mirrored = dirigent_sync::Repo::open(&tmp.path().join("m"))
            .await
            .unwrap();
        let mut files = dirigent_sync::Workspace::new(&had);
        let snapshot = mirrored
            .snapshot(&mut files, None, "Write c1")
            .await
            .unwrap()
            .unwrap();
        let files = dirigent_sync::events(&snapshot);

        for source in [Some(git_source(&url, None)), None] {
            // on another machine, with no checkpoint to restore
            let work = tempfile::tempdir().unwrap();
            let mut session = Logged {
                log: ready(false),
                files: files.clone(),
                published: Vec::new(),
            };
            open(
                &mut session,
                leak(work.path()),
                source.as_ref(),
                None,
                &tmp.path().join(format!("mirror-{}", source.is_some())),
                Secrets::none(),
            )
            .await
            .unwrap();
            let [Event::WorkspaceReady {
                recreated: true,
                files: Some(commit),
                ..
            }] = &session.published[..]
            else {
                panic!("{:?}", session.published)
            };
            assert_eq!(commit, &snapshot.commit);
            assert_eq!(
                std::fs::read_to_string(work.path().join("file")).unwrap(),
                "changed\n"
            );
            assert_eq!(
                std::fs::read_to_string(work.path().join("new")).unwrap(),
                "n"
            );
        }
    }

    #[tokio::test]
    async fn a_workspace_kept_while_the_session_ran_elsewhere_gets_what_it_did_there() {
        let tmp = tempfile::tempdir().unwrap();
        let (here, there) = (tmp.path().join("here"), tmp.path().join("there"));
        std::fs::create_dir(&here).unwrap();
        std::fs::create_dir(&there).unwrap();
        let (here, there) = (leak(&here), leak(&there));
        let read = |dir: &Path, name: &str| std::fs::read_to_string(dir.join(name)).ok();
        let mut files = Vec::new();
        // runs here (a host), starting from what's there; its player mirrors it, then leaves
        std::fs::write(here.join("file"), "here\n").unwrap();
        std::fs::write(here.join("deleted"), "d").unwrap();
        let mut session = Logged {
            log: ready(false),
            files: files.clone(),
            published: Vec::new(),
        };
        let mirror = tmp.path().join("mirror-here");
        open(&mut session, here, None, None, &mirror, Secrets::none())
            .await
            .unwrap();
        files.append(&mut session.published);
        assert!(!files.is_empty());

        // runs elsewhere: its workspace from the log, then changed there
        let mut session = Logged {
            log: ready(false),
            files: files.clone(),
            published: Vec::new(),
        };
        let (_, m) = open(
            &mut session,
            there,
            None,
            None,
            &tmp.path().join("mirror-there"),
            Secrets::none(),
        )
        .await
        .unwrap();
        assert_eq!(read(there, "file").as_deref(), Some("here\n"));
        std::fs::write(there.join("file"), "there\n").unwrap();
        std::fs::remove_file(there.join("deleted")).unwrap();
        std::fs::write(there.join("new"), "n").unwrap();
        files.extend(m.unwrap().snapshot("Write").await);

        // back here, where the workspace stayed as this host's player left it
        let mut session = Logged {
            log: ready(false),
            files: files.clone(),
            published: Vec::new(),
        };
        open(&mut session, here, None, None, &mirror, Secrets::none())
            .await
            .unwrap();
        assert_eq!(read(here, "file").as_deref(), Some("there\n"));
        assert_eq!(read(here, "deleted"), None);
        assert_eq!(read(here, "new").as_deref(), Some("n"));
        // it's the log's last commit: nothing new to mirror, and no event says it was made
        assert!(session.published.is_empty(), "{:?}", session.published);

        // and when nothing happened elsewhere, what's here (not mirrored yet) stays
        std::fs::write(here.join("file"), "changed here\n").unwrap();
        let mut session = Logged {
            log: ready(false),
            files: files.clone(),
            published: Vec::new(),
        };
        open(&mut session, here, None, None, &mirror, Secrets::none())
            .await
            .unwrap();
        assert_eq!(read(here, "file").as_deref(), Some("changed here\n"));
        assert!(matches!(
            &session.published[..],
            [Event::FilesChanged { .. }, ..]
        ));
    }
}
