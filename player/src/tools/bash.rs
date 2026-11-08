use std::process::Stdio;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{opt_u64, req_str, truncate_middle, Tool, ToolCtx, ToolOutput};
use crate::provider::ToolSpec;

pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const MAX_TIMEOUT_MS: u64 = 600_000;
pub const MAX_OUTPUT_CHARS: usize = 30_000;
pub const HEAD_CHARS: usize = 20_000;

/// The player's variables a command gets: what a shell and the usual tools need, never its
/// own (its credentials, the stack's addresses).
const PASSED_ON: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "TERM", "TMPDIR", "TZ",
];

fn passed_on(key: &str) -> bool {
    PASSED_ON.contains(&key) || key.starts_with("LC_")
}

pub struct Bash;

#[async_trait]
impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Bash".into(),
            description: "Executes a bash command in the working directory and returns its combined stdout and stderr. Each call runs in a fresh shell: the working directory and environment reset between calls, so chain dependent commands with && in one call. Use absolute paths and quote paths containing spaces. Default timeout is 120000 ms; the maximum is 600000 ms. Output longer than 30000 characters is truncated in the middle. Do not use this tool for reading or searching files: use Read, Glob and Grep instead. Never run destructive commands (git reset --hard, rm -rf on project paths, force pushes) unless explicitly asked.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The command to execute" },
                    "timeout": { "type": "number", "description": "Optional timeout in milliseconds (max 600000)" },
                    "description": { "type": "string", "description": "Clear, concise description of what this command does in active voice, 5-10 words" }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        }
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn call(&self, input: Value, ctx: &ToolCtx, cancel: CancellationToken) -> ToolOutput {
        let command = match req_str(&input, "command") {
            Ok(c) => c.to_string(),
            Err(e) => return ToolOutput::error(e),
        };
        let timeout = Duration::from_millis(
            opt_u64(&input, "timeout")
                .unwrap_or(DEFAULT_TIMEOUT_MS)
                .clamp(1, MAX_TIMEOUT_MS),
        );

        let mut cmd = Command::new("bash");
        // the player's environment only as far as a shell needs it, and the session's secrets
        cmd.env_clear();
        for (key, value) in std::env::vars_os() {
            if passed_on(&key.to_string_lossy()) {
                cmd.env(key, value);
            }
        }
        cmd.envs(ctx.secrets.current());
        cmd.arg("-c")
            .arg(&command)
            .current_dir(ctx.workdir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0) // own group, so a timeout kills children too
            .kill_on_drop(true);
        let started = Instant::now();
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(format!("Failed to spawn bash: {e}")),
        };
        let pid = child.id();

        // Interleave stdout and stderr in arrival order.
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let mut pumps = tokio::task::JoinSet::new();
        if let Some(out) = child.stdout.take() {
            pumps.spawn(pump(out, tx.clone()));
        }
        if let Some(err) = child.stderr.take() {
            pumps.spawn(pump(err, tx));
        } else {
            drop(tx);
        }

        let mut output = String::new();
        let mut timed_out = false;
        let mut cancelled = false;
        let status = tokio::select! {
            s = child.wait() => s.ok(),
            _ = tokio::time::sleep(timeout) => {
                timed_out = true;
                if let Some(pid) = pid { kill_group(pid); }
                child.wait().await.ok()
            }
            _ = cancel.cancelled() => {
                cancelled = true;
                if let Some(pid) = pid { kill_group(pid); }
                child.wait().await.ok()
            }
        };
        // Pumps finish once the pipes close (the group kill closes them).
        while pumps.join_next().await.is_some() {}
        while let Ok(chunk) = rx.try_recv() {
            output.push_str(&chunk);
        }
        let duration_ms = started.elapsed().as_millis() as u64;
        let exit_code = status.and_then(|s| s.code());
        let (mut text, truncated) =
            truncate_middle(output.trim_end(), MAX_OUTPUT_CHARS, HEAD_CHARS);
        let meta = json!({
            "exit_code": exit_code,
            "duration_ms": duration_ms,
            "truncated": truncated,
            "timed_out": timed_out,
            "cancelled": cancelled,
        });
        if cancelled {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str("Command aborted by user interrupt");
            return ToolOutput::error(text).with_meta(meta);
        }
        if timed_out {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&format!(
                "Command timed out after {}s",
                timeout.as_secs_f64()
            ));
            return ToolOutput::error(text).with_meta(meta);
        }
        match exit_code {
            Some(0) => ToolOutput::ok(text).with_meta(meta),
            Some(code) => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&format!("Exit code: {code}"));
                ToolOutput::error(text).with_meta(meta)
            }
            None => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str("Process was killed by a signal");
                ToolOutput::error(text).with_meta(meta)
            }
        }
    }
}

async fn pump<R: tokio::io::AsyncRead + Unpin>(reader: R, tx: mpsc::UnboundedSender<String>) {
    let mut lines = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match lines.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if tx.send(String::from_utf8_lossy(&buf).into_owned()).is_err() {
                    break;
                }
            }
        }
    }
}

fn kill_group(pid: u32) {
    // SAFETY: plain libc call with a pid we spawned; negative pid targets the group.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secrets::Secrets;
    use crate::tools::testing::*;

    async fn bash(ctx: &ToolCtx, input: Value) -> ToolOutput {
        Bash.call(input, ctx, CancellationToken::new()).await
    }

    #[tokio::test]
    async fn captures_output_cwd_and_exit_code() {
        let (_d, ctx) = ctx();
        let out = bash(&ctx, json!({ "command": "echo out; echo err >&2; pwd" })).await;
        assert!(!out.is_error, "{}", out.content);
        // stdout and stderr are interleaved in arrival order, so compare as a set.
        let mut lines: Vec<&str> = out.content.lines().collect();
        lines.sort();
        assert_eq!(lines, vec![ctx.workdir.to_str().unwrap(), "err", "out"]);
        assert_eq!(out.meta["exit_code"], 0);

        let out = bash(&ctx, json!({ "command": "echo boom; exit 3" })).await;
        assert!(out.is_error);
        assert_eq!(out.content, "boom\nExit code: 3");
        assert_eq!(out.meta["exit_code"], 3);
    }

    #[tokio::test]
    async fn a_command_gets_the_secrets_and_none_of_the_players_own_variables() {
        let dir = tempfile::tempdir().unwrap();
        let root: &'static std::path::Path =
            Box::leak(dir.path().canonicalize().unwrap().into_boxed_path());
        let (tx, secrets) = Secrets::channel(&[("GITHUB_TOKEN", "ghp_first")]);
        let ctx = ToolCtx::new(root, secrets);
        // the player's own, which no command should see
        assert!(!passed_on("DIRIGENT_KOPIA_TOKEN") && !passed_on("ANTHROPIC_API_KEY"));
        assert!(passed_on("PATH") && passed_on("LC_ALL"));
        let env = |out: ToolOutput| -> Vec<String> {
            let mut names: Vec<String> = out
                .content
                .lines()
                .filter_map(|l| l.split_once('=').map(|(k, _)| k.to_string()))
                .collect();
            names.sort();
            names
        };
        let out = bash(&ctx, json!({ "command": "env -0 | tr '\\0' '\\n'" })).await;
        let names = env(out);
        assert!(
            names.iter().all(|n| passed_on(n)
                || n == "GITHUB_TOKEN"
                || n == "PWD"
                || n == "SHLVL"
                || n == "_"
                || n == "OLDPWD"),
            "{names:?}"
        );
        assert!(names.contains(&"GITHUB_TOKEN".to_string()));
        let out = bash(&ctx, json!({ "command": "printf %s \"$GITHUB_TOKEN\"" })).await;
        assert_eq!(out.content, "ghp_first");
        // one given meanwhile reaches the next command
        tx.send_replace(crate::secrets::tokens(&[("GITHUB_TOKEN", "ghp_second")]));
        let out = bash(&ctx, json!({ "command": "printf %s \"$GITHUB_TOKEN\"" })).await;
        assert_eq!(out.content, "ghp_second");
    }

    #[tokio::test]
    async fn timeout_kills_process_group() {
        let (_d, ctx) = ctx();
        let started = Instant::now();
        let out = bash(
            &ctx,
            json!({ "command": "echo started; sleep 30; echo never", "timeout": 300 }),
        )
        .await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(out.is_error);
        assert!(out
            .content
            .starts_with("started\nCommand timed out after 0.3s"));
        assert!(!out.content.contains("never"));
        assert_eq!(out.meta["timed_out"], true);
    }

    #[tokio::test]
    async fn cancel_kills_process_group() {
        let (_d, ctx) = ctx();
        let token = CancellationToken::new();
        let t2 = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            t2.cancel();
        });
        let started = Instant::now();
        let out = Bash
            .call(
                json!({ "command": "echo started; sleep 30; echo never" }),
                &ctx,
                token,
            )
            .await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(out.is_error);
        assert_eq!(out.content, "started\nCommand aborted by user interrupt");
        assert_eq!(out.meta["cancelled"], true);
    }

    #[tokio::test]
    async fn truncates_long_output() {
        let (_d, ctx) = ctx();
        let out = bash(&ctx, json!({ "command": "yes abcdefghij | head -c 50000" })).await;
        assert!(!out.is_error);
        assert!(out.content.contains("chars truncated ...]"));
        assert!(out.content.len() < 31_000);
        assert_eq!(out.meta["truncated"], true);
    }
}
