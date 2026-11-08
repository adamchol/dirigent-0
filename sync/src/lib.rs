//! A session's workspace as git commits carried in its log ([`Event::FilesChanged`]), so whoever
//! reads the log can have the workspace's files: the player commits the workspace after each
//! tool call that changed it, and a bundle of what's new since the commit before goes into the
//! log; the CLI (or a player restoring the workspace) fetches the bundles in order and checks the
//! last one out.
//!
//! The repository is this crate's own, kept apart from the workspace (whose own `.git`, if it
//! has one, is never read or written), and run with no system or user config. What's in a
//! commit is what checkpoints keep: the files `.gitignore`s don't leave out, `.git` folders
//! aside, up to [`MAX_FILE_SIZE`] each ([`Workspace`]).
//!
//! [`Event::FilesChanged`]: dirigent_protocol::Event::FilesChanged

mod bundles;
mod workspace;

use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Context;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub use bundles::{events, Bundle, Bundles, PART};
pub use workspace::{Workspace, MAX_FILE_SIZE};

/// How long one git command may take.
const TIMEOUT: Duration = Duration::from_secs(300);
/// The tree with nothing in it.
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// The last commit made or fetched, which the next bundle is made against.
const LIVE: &str = "refs/heads/live";
/// The commit the work tree was last checked out at (the index is its tree).
const CHECKED_OUT: &str = "refs/heads/checkout";

/// A commit of the workspace, and its bundle: the objects new since `parent`.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub commit: String,
    pub parent: Option<String>,
    pub bundle: Vec<u8>,
}

/// What a checkout did: the paths it wrote or deleted (as the last checkout here had them), and
/// those of them it moved aside first.
#[derive(Debug, Clone, PartialEq)]
pub struct Checkout {
    pub changed: Vec<PathBuf>,
    pub moved: Vec<PathBuf>,
}

pub struct Repo {
    dir: PathBuf,
}

impl Repo {
    /// The repository in `dir`, made there if it isn't yet.
    pub async fn open(dir: &Path) -> anyhow::Result<Repo> {
        std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
        let repo = Repo {
            dir: dir.canonicalize()?,
        };
        if !repo.dir.join("HEAD").exists() {
            repo.run(repo.git().args(["init", "--quiet", "--bare"]))
                .await
                .context("making the sync repository")?;
            for (key, value) in [
                // a work tree comes with each command that needs one
                ("core.bare", "false"),
                // fetched objects are referenced by no ref but `live`'s history
                ("gc.auto", "0"),
                ("core.autocrlf", "false"),
                ("core.symlinks", "true"),
                ("core.filemode", "true"),
            ] {
                repo.run(repo.git().args(["config", key, value])).await?;
            }
        }
        Ok(repo)
    }

    /// Whether the repository has `commit`.
    pub async fn has(&self, commit: &str) -> bool {
        let mut cmd = self.git();
        cmd.args(["cat-file", "-e", &format!("{commit}^{{commit}}")]);
        self.run(&mut cmd).await.is_ok()
    }

    /// Fetch `bundle`'s objects, which needs the commit it was made against; `live` becomes its
    /// commit.
    pub async fn fetch(&self, bundle: &Bundle) -> anyhow::Result<()> {
        if !self.has(&bundle.commit).await {
            let path = self.scratch("bundle");
            std::fs::write(&path, &bundle.bundle)?;
            let mut cmd = self.git();
            cmd.args(["bundle", "unbundle"]).arg(&path);
            let out = self.run(&mut cmd).await;
            let _ = std::fs::remove_file(&path);
            out.with_context(|| format!("fetching commit {}", bundle.commit))?;
        }
        self.run(self.git().args(["update-ref", LIVE, &bundle.commit]))
            .await?;
        Ok(())
    }

    /// Fetch `bundles` in order, up to the first that doesn't apply (which, with every bundle
    /// after it, is left out); the last commit fetched, if any, and why one didn't apply.
    pub async fn replay(
        &self,
        bundles: impl IntoIterator<Item = Bundle>,
    ) -> (Option<String>, Option<anyhow::Error>) {
        let mut last = None;
        for bundle in bundles {
            if let Err(e) = self.fetch(&bundle).await {
                return (last, Some(e));
            }
            last = Some(bundle.commit);
        }
        (last, None)
    }

    /// Commit `work` as `files` reads it, on top of `parent`; `None` when its files are as
    /// `parent` has them.
    pub async fn snapshot(
        &self,
        files: &mut Workspace,
        parent: Option<&str>,
        message: &str,
    ) -> anyhow::Result<Option<Snapshot>> {
        let entries = files.read(self).await?;
        let index = self.scratch("index");
        let mut list = Vec::new();
        for (mode, oid, path) in &entries {
            list.extend_from_slice(format!("{mode:o} {oid}\t").as_bytes());
            list.extend_from_slice(path.as_os_str().as_bytes());
            list.push(0);
        }
        let mut cmd = self.git();
        cmd.env("GIT_INDEX_FILE", &index)
            .args(["update-index", "-z", "--index-info"]);
        let made = self.input(&mut cmd, list).await;
        let tree = match made {
            Ok(_) => {
                let mut cmd = self.git();
                cmd.env("GIT_INDEX_FILE", &index).arg("write-tree");
                self.line(&mut cmd).await
            }
            Err(e) => Err(e),
        };
        let _ = std::fs::remove_file(&index);
        let tree = tree.context("writing the workspace's tree")?;
        if let Some(parent) = parent {
            if self.tree(parent).await? == tree {
                return Ok(None);
            }
        }
        let mut cmd = self.git();
        cmd.args(["commit-tree", &tree, "-m", message]);
        if let Some(parent) = parent {
            cmd.args(["-p", parent]);
        }
        let commit = self.line(&mut cmd).await?;
        self.run(self.git().args(["update-ref", LIVE, &commit]))
            .await?;
        let path = self.scratch("bundle");
        let mut cmd = self.git();
        cmd.args(["bundle", "create", "--quiet"])
            .arg(&path)
            .arg(LIVE);
        if let Some(parent) = parent {
            cmd.arg(format!("^{parent}"));
        }
        let made = self.run(&mut cmd).await;
        let bundle = made.and_then(|_| Ok(std::fs::read(&path)?));
        let _ = std::fs::remove_file(&path);
        Ok(Some(Snapshot {
            commit,
            parent: parent.map(str::to_string),
            bundle: bundle.context("bundling the commit")?,
        }))
    }

    /// Make `work`'s files `commit`'s: what changed since the last checkout is written, what's
    /// gone is deleted, and nothing else is touched (files neither commit has, such as what
    /// builds make, stay). With `keep_local`, a file that would be overwritten or deleted while
    /// it isn't as the last checkout wrote it is first moved aside to `<path>.local`. Without,
    /// it's overwritten, and files `commit` doesn't have that a [`Workspace`] would read are
    /// deleted too (a workspace restored from elsewhere, which no checkout here wrote).
    pub async fn checkout(
        &self,
        work: &Path,
        commit: &str,
        keep_local: bool,
    ) -> anyhow::Result<Checkout> {
        let old = self.resolve(CHECKED_OUT).await;
        let from = match &old {
            Some(old) => self.tree(old).await?,
            None => EMPTY_TREE.to_string(),
        };
        let to = self.tree(commit).await?;
        let mut cmd = self.git();
        cmd.args([
            "diff-tree",
            "-r",
            "-z",
            "--name-only",
            "--no-renames",
            &from,
            &to,
        ]);
        let changed = paths(&self.run(&mut cmd).await?);
        let mut moved = Vec::new();
        if keep_local {
            if !changed.is_empty() {
                moved = self.move_aside(work, commit, &changed).await?;
            }
        } else {
            let mut cmd = self.git();
            cmd.args(["ls-tree", "-r", "-z", "--name-only", commit]);
            let kept: HashSet<PathBuf> = paths(&self.run(&mut cmd).await?).into_iter().collect();
            let found = Workspace::new(work).list().await?;
            for path in found.into_iter().filter(|p| !kept.contains(p)) {
                let _ = std::fs::remove_file(work.join(path));
            }
        }
        let mut cmd = self.git();
        cmd.env("GIT_WORK_TREE", work).current_dir(work);
        match (keep_local, &old) {
            // from the last checkout to this one: only what changed between them is written,
            // and git refuses to lose what `move_aside` didn't move
            (true, Some(old)) => cmd.args(["read-tree", "-m", "-u", old, commit]),
            (true, None) => cmd.args(["read-tree", "-m", "-u", commit]),
            (false, _) => cmd.args(["read-tree", "--reset", "-u", commit]),
        };
        self.run(&mut cmd)
            .await
            .with_context(|| format!("checking out {commit}"))?;
        self.run(self.git().args(["update-ref", CHECKED_OUT, commit]))
            .await?;
        Ok(Checkout { changed, moved })
    }

    /// Of `changed` (paths a checkout of `commit` writes or deletes), move aside those whose
    /// file in `work` isn't what the last checkout left there.
    async fn move_aside(
        &self,
        work: &Path,
        commit: &str,
        changed: &[PathBuf],
    ) -> anyhow::Result<Vec<PathBuf>> {
        let mut cmd = self.git();
        cmd.env("GIT_WORK_TREE", work)
            .current_dir(work)
            .args(["update-index", "-q", "--refresh"]);
        // it fails when files need updating, which `diff-files` then tells
        let _ = self.run(&mut cmd).await;
        let mut cmd = self.git();
        cmd.env("GIT_WORK_TREE", work)
            .current_dir(work)
            .args(["diff-files", "-z", "--name-only"]);
        let modified: HashSet<PathBuf> = paths(&self.run(&mut cmd).await?).into_iter().collect();
        let mut cmd = self.git();
        cmd.args(["ls-files", "-z"]);
        let tracked: HashSet<PathBuf> = paths(&self.run(&mut cmd).await?).into_iter().collect();
        let mut moved = Vec::new();
        for path in changed {
            let full = work.join(path);
            let Ok(meta) = std::fs::symlink_metadata(&full) else {
                continue;
            };
            let differs = if tracked.contains(path) {
                modified.contains(path)
            } else if meta.is_dir() {
                true
            } else {
                // there before any checkout wrote it: kept unless it's what comes, which is
                // written again (git won't write over a file it didn't)
                let local = self.hash(&full, &meta).await?;
                let mut cmd = self.git();
                let spec = format!("{commit}:{}", path.display());
                cmd.args(["rev-parse", "--verify", "-q", &spec]);
                let same = self.line(&mut cmd).await.ok() == Some(local);
                if same {
                    std::fs::remove_file(&full)?;
                }
                !same
            };
            if differs {
                let mut aside = full.clone().into_os_string();
                aside.push(".local");
                let aside = PathBuf::from(aside);
                if std::fs::symlink_metadata(&aside).is_ok_and(|m| m.is_dir()) {
                    std::fs::remove_dir_all(&aside)?;
                }
                std::fs::rename(&full, &aside)
                    .with_context(|| format!("moving {} aside", full.display()))?;
                moved.push(path.clone());
            }
        }
        Ok(moved)
    }

    /// The blob id of the file (or link) at `path`, without storing it.
    async fn hash(&self, path: &Path, meta: &std::fs::Metadata) -> anyhow::Result<String> {
        let mut cmd = self.git();
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(path)?;
            cmd.args(["hash-object", "--stdin"]);
            return self
                .input(&mut cmd, target.as_os_str().as_bytes().to_vec())
                .await
                .map(|out| String::from_utf8_lossy(&out).trim().to_string());
        }
        cmd.args(["hash-object", "--no-filters"]).arg(path);
        self.line(&mut cmd).await
    }

    async fn tree(&self, commit: &str) -> anyhow::Result<String> {
        let mut cmd = self.git();
        cmd.args(["rev-parse", &format!("{commit}^{{tree}}")]);
        self.line(&mut cmd).await
    }

    async fn resolve(&self, name: &str) -> Option<String> {
        let mut cmd = self.git();
        cmd.args(["rev-parse", "--verify", "-q", name]);
        self.line(&mut cmd).await.ok()
    }

    /// A path in the repository for a file of one command's.
    fn scratch(&self, what: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        self.dir
            .join(format!("dirigent-{what}-{}-{n}", std::process::id()))
    }

    fn git(&self) -> Command {
        let mut cmd = Command::new("git");
        cmd.env("GIT_DIR", &self.dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "dirigent")
            .env("GIT_AUTHOR_EMAIL", "dirigent@localhost")
            .env("GIT_COMMITTER_NAME", "dirigent")
            .env("GIT_COMMITTER_EMAIL", "dirigent@localhost")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }

    async fn run(&self, cmd: &mut Command) -> anyhow::Result<Vec<u8>> {
        self.input(cmd, Vec::new()).await
    }

    async fn line(&self, cmd: &mut Command) -> anyhow::Result<String> {
        let out = self.run(cmd).await?;
        Ok(String::from_utf8_lossy(&out).trim().to_string())
    }

    /// Its stdout, given `input` on its stdin.
    async fn input(&self, cmd: &mut Command, input: Vec<u8>) -> anyhow::Result<Vec<u8>> {
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .take(2)
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let what = format!("git {}", args.join(" "));
        if !input.is_empty() {
            cmd.stdin(Stdio::piped());
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("running {what} (is git installed?)"))?;
        let mut stdin = child.stdin.take();
        let write = async {
            if let Some(stdin) = &mut stdin {
                stdin.write_all(&input).await?;
                stdin.shutdown().await?;
            }
            drop(stdin);
            Ok::<_, std::io::Error>(())
        };
        let out = tokio::time::timeout(TIMEOUT, async {
            let (written, out) = tokio::join!(write, child.wait_with_output());
            written?;
            out
        })
        .await
        .map_err(|_| anyhow::anyhow!("{what} timed out after {}s", TIMEOUT.as_secs()))?
        .with_context(|| format!("running {what}"))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            anyhow::bail!("{what}: {} ({})", err.trim(), out.status);
        }
        Ok(out.stdout)
    }
}

/// NUL-separated paths.
fn paths(out: &[u8]) -> Vec<PathBuf> {
    out.split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| PathBuf::from(OsStr::from_bytes(p)))
        .collect()
}

#[cfg(test)]
mod tests;
