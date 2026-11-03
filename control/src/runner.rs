//! Where control starts the players it runs: here, as local processes (Kubernetes pods later).
//! Only starting is the runner's: a player's liveness is its lease, and it stops on control's
//! word (`Detach`), so the runner neither watches nor kills it.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::Context;
use tokio::process::Command;
use tracing::{info, warn};
use uuid::Uuid;

pub trait Runner: Send + Sync {
    /// Which runner it is, in the leases of the players it started: `local:<root>`. A control
    /// restarts only those of its own runner.
    fn id(&self) -> &str;

    /// Start `player` (an id) on `session`, where the lease for its `epoch` waits for it.
    fn start(&self, session: Uuid, player: &str, epoch: i64) -> anyhow::Result<()>;
}

/// The environment a player gets: what it needs, and none of control's own (its Postgres).
const PASSED_ON: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "RUST_LOG",
    "CHLIVE_URL",
    "DIRIGENT_KOPIA_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_BASE_URL",
];

/// Players as processes on this machine, each session's in `<root>/<session>`: its workspace,
/// the player's state, and its output (`player.log`). They outlive a control that stops (and
/// resume their leases when it's back), as pods would.
pub struct Local {
    pub id: String,
    pub root: PathBuf,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Control's WebSocket for players.
    pub control_url: String,
}

impl Runner for Local {
    fn id(&self) -> &str {
        &self.id
    }

    fn start(&self, session: Uuid, player: &str, epoch: i64) -> anyhow::Result<()> {
        let dir = self.root.join(session.to_string());
        std::fs::create_dir_all(&dir).with_context(|| format!("making {}", dir.display()))?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("player.log"))
            .with_context(|| format!("opening {}/player.log", dir.display()))?;
        let mut cmd = Command::new(&self.program);
        cmd.arg("--session")
            .arg(session.to_string())
            .args(["--player-id", player])
            .args(["--epoch", &epoch.to_string()])
            .args(["--control-url", &self.control_url])
            .arg("--workdir")
            .arg(dir.join("workspace"))
            .arg("--state-dir")
            .arg(dir.join("state"))
            .args(&self.args)
            .env_clear()
            .envs(
                PASSED_ON
                    .iter()
                    .filter_map(|k| Some((k, std::env::var_os(k)?))),
            )
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("running {}", self.program.display()))?;
        let pid = child.id();
        info!(session = %session, player, epoch, pid, "player started");
        let player = player.to_string();
        tokio::spawn(async move {
            match child.wait().await {
                Ok(status) if status.success() => {
                    info!(session = %session, player, "player exited")
                }
                Ok(status) => warn!(session = %session, player, %status, "player failed"),
                Err(e) => warn!(session = %session, player, "waiting for the player: {e}"),
            }
        });
        Ok(())
    }
}
