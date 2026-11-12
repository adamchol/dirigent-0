//! Checkpoints of the workspace: a kopia snapshot of it after every turn, which a player that
//! finds the workspace gone restores (see `workspace`). The player only reads the workspace to
//! snapshot it; what's left out (`.gitignore`d files, big ones) and how long snapshots are kept
//! is the kopia repository's policy, set where the repository is made.
//!
//! The player connects to the repository on its own, with a config and cache of its own outside
//! the workspace, as `<session>@dirigent`: every snapshot of a session is of one kopia source,
//! whatever machine took it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;
use tokio::process::Command;

use crate::process::{command, run};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(600);

pub struct Checkpoints {
    /// The player's kopia state: its config, cache and logs.
    state: PathBuf,
    session: String,
}

impl Checkpoints {
    /// Connected to the repository `token` (`kopia repository status -t`) for `session`, with
    /// its state in `state`.
    pub async fn connect(token: &str, state: &Path, session: &str) -> anyhow::Result<Self> {
        std::fs::create_dir_all(state)
            .with_context(|| format!("making the kopia state in {}", state.display()))?;
        let checkpoints = Checkpoints {
            state: state.to_path_buf(),
            session: session.to_string(),
        };
        // a config left by an earlier run, which connecting again would refuse
        let _ = std::fs::remove_file(state.join("kopia.config"));
        let mut cmd = checkpoints.kopia();
        cmd.args(["repository", "connect", "from-config", "--token", token])
            .arg("--cache-directory")
            .arg(state.join("cache"))
            .args(["--override-username", session])
            .args(["--override-hostname", "dirigent"]);
        run(cmd, CONNECT_TIMEOUT)
            .await
            .map_err(|e| anyhow::anyhow!("connecting to the checkpoints' repository: {e}"))?;
        Ok(checkpoints)
    }

    /// Snapshot `workdir` after the turn `turn`; the snapshot's id.
    pub async fn snapshot(&self, workdir: &Path, turn: &str) -> Result<String, String> {
        #[derive(Deserialize)]
        struct Manifest {
            id: String,
        }
        let mut cmd = self.kopia();
        cmd.args(["snapshot", "create", "--json"])
            .args(["--description", &format!("turn {turn}")])
            .args(["--tags", &format!("session:{}", self.session)])
            .arg(workdir);
        let out = run(cmd, SNAPSHOT_TIMEOUT)
            .await
            .map_err(|e| format!("snapshotting the workspace: {e}"))?;
        let manifest: Manifest = serde_json::from_str(&out)
            .map_err(|e| format!("reading kopia's snapshot: {e}: {out}"))?;
        Ok(manifest.id)
    }

    /// Restore the snapshot `snapshot` into `workdir`.
    pub async fn restore(&self, snapshot: &str, workdir: &Path) -> Result<(), String> {
        let mut cmd = self.kopia();
        cmd.args(["snapshot", "restore", snapshot]).arg(workdir);
        run(cmd, SNAPSHOT_TIMEOUT)
            .await
            .map(drop)
            .map_err(|e| format!("restoring the snapshot {snapshot}: {e}"))
    }

    fn kopia(&self) -> Command {
        let mut cmd = command("kopia");
        cmd.env("KOPIA_CONFIG_PATH", self.state.join("kopia.config"))
            .env("KOPIA_LOG_DIR", self.state.join("logs"))
            .env("KOPIA_CHECK_FOR_UPDATES", "false");
        cmd
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A kopia repository in `dir` (on the filesystem) with the stack's policy (nix/shell.nix,
    /// `kopia-repo`); its token.
    pub(crate) fn repo(dir: &Path) -> String {
        let kopia = |args: &[&str]| {
            let out = std::process::Command::new("kopia")
                .args(args)
                .env("KOPIA_CONFIG_PATH", dir.join("admin.config"))
                .env("KOPIA_LOG_DIR", dir.join("logs"))
                .env("KOPIA_CHECK_FOR_UPDATES", "false")
                .output()
                .unwrap();
            assert!(out.status.success(), "kopia {args:?}: {out:?}");
            String::from_utf8(out.stdout).unwrap()
        };
        let path = dir.join("repo");
        kopia(&[
            "repository",
            "create",
            "filesystem",
            "--path",
            path.to_str().unwrap(),
            "--password",
            "pw",
        ]);
        kopia(&[
            "policy",
            "set",
            "--global",
            "--add-dot-ignore",
            ".gitignore",
            "--max-file-size",
            "0",
        ]);
        let status = kopia(&["repository", "status", "-t", "-s"]);
        status
            .split_whitespace()
            .skip_while(|w| *w != "--token")
            .nth(1)
            .expect("a token")
            .to_string()
    }

    pub(crate) async fn checkpoints(dir: &Path) -> Checkpoints {
        let token = repo(dir);
        Checkpoints::connect(&token, &dir.join("state"), "s")
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn each_snapshot_restores_the_workspace_as_it_was() {
        let tmp = tempfile::tempdir().unwrap();
        let cp = checkpoints(tmp.path()).await;
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::write(ws.join("file"), "one").unwrap();
        std::fs::write(ws.join(".git/HEAD"), "ref").unwrap();
        let first = cp.snapshot(&ws, "t1").await.unwrap();
        std::fs::write(ws.join("file"), "two").unwrap();
        let second = cp.snapshot(&ws, "t2").await.unwrap();
        assert_ne!(first, second);
        for (snapshot, want) in [(&first, "one"), (&second, "two")] {
            let to = tmp.path().join(snapshot.as_str());
            std::fs::create_dir(&to).unwrap();
            cp.restore(snapshot, &to).await.unwrap();
            assert_eq!(std::fs::read_to_string(to.join("file")).unwrap(), want);
            assert_eq!(
                std::fs::read_to_string(to.join(".git/HEAD")).unwrap(),
                "ref"
            );
        }
    }

    #[tokio::test]
    async fn ignored_files_are_left_out_and_big_ones_kept() {
        let tmp = tempfile::tempdir().unwrap();
        let cp = checkpoints(tmp.path()).await;
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join("target")).unwrap();
        std::fs::write(ws.join(".gitignore"), "target/\n").unwrap();
        std::fs::write(ws.join("target/out"), "built").unwrap();
        // past what the log carries: only a checkpoint brings it back
        let big = std::fs::File::create(ws.join("big")).unwrap();
        big.set_len(dirigent_sync::MAX_FILE_SIZE + 1).unwrap();
        std::fs::write(ws.join("kept"), "x").unwrap();
        let snapshot = cp.snapshot(&ws, "t").await.unwrap();
        let to = tmp.path().join("to");
        std::fs::create_dir(&to).unwrap();
        cp.restore(&snapshot, &to).await.unwrap();
        assert!(to.join("kept").exists());
        assert!(!to.join("target/out").exists());
        assert_eq!(
            std::fs::metadata(to.join("big")).unwrap().len(),
            dirigent_sync::MAX_FILE_SIZE + 1
        );
    }

    #[tokio::test]
    async fn an_unknown_snapshot_fails_to_restore() {
        let tmp = tempfile::tempdir().unwrap();
        let cp = checkpoints(tmp.path()).await;
        let err = cp
            .restore("0123456789abcdef", &tmp.path().join("to"))
            .await
            .unwrap_err();
        assert!(err.starts_with("restoring the snapshot"), "{err}");
    }
}
