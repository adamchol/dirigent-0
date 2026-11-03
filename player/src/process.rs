//! The programs the player runs itself, not as the agent's tools: `git` to make a workspace,
//! `kopia` to checkpoint and restore it.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// How much of a failure's stderr is kept.
const ERROR_TAIL: usize = 2000;

/// `program`, with no input and its output captured, killed if it's dropped.
pub fn command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

/// Its stdout, or the tail of its stderr.
pub async fn run(mut cmd: Command, timeout: Duration) -> Result<String, String> {
    let program = cmd.as_std().get_program().to_string_lossy().into_owned();
    let out = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| format!("timed out after {}s", timeout.as_secs()))?
        .map_err(|e| format!("running {program}: {e}"))?;
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
