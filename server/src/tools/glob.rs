use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{opt_str, req_str, Tool, ToolCtx, ToolOutput};
use crate::provider::ToolSpec;

pub const MAX_RESULTS: usize = 100;

pub struct Glob;

#[async_trait]
impl Tool for Glob {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Glob".into(),
            description: "Fast file pattern matching tool that works with any codebase size. Supports glob patterns like \"**/*.js\" or \"src/**/*.ts\". Returns matching file paths sorted by modification time, newest first. Respects .gitignore and skips hidden files. Use this tool when you need to find files by name patterns instead of find or ls via Bash.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "The glob pattern to match files against" },
                    "path": { "type": "string", "description": "The directory to search in. If not specified, the working directory will be used. Must be an absolute path." }
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolCtx, _cancel: CancellationToken) -> ToolOutput {
        let pattern = match req_str(&input, "pattern") {
            Ok(p) => p.to_string(),
            Err(e) => return ToolOutput::error(e),
        };
        let root = match opt_str(&input, "path") {
            Some(p) => match ctx.resolve(p) {
                Ok(p) => p,
                Err(e) => return ToolOutput::error(e),
            },
            None => ctx.workdir.to_path_buf(),
        };
        if !root.is_dir() {
            return ToolOutput::error(format!("Path is not a directory: {}", root.display()));
        }
        let result = tokio::task::spawn_blocking(move || search(&root, &pattern)).await;
        match result {
            Ok(Ok((paths, truncated))) => {
                if paths.is_empty() {
                    return ToolOutput::ok("No files found");
                }
                let mut out = paths
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                if truncated {
                    out.push_str(
                        "\n(Results are truncated. Consider using a more specific path or pattern.)",
                    );
                }
                ToolOutput::ok(out)
                    .with_meta(json!({ "count": paths.len(), "truncated": truncated }))
            }
            Ok(Err(e)) => ToolOutput::error(e),
            Err(e) => ToolOutput::error(format!("Glob failed: {e}")),
        }
    }
}

pub fn search(root: &Path, pattern: &str) -> Result<(Vec<PathBuf>, bool), String> {
    let glob = globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
        .map_err(|e| format!("Invalid glob pattern: {e}"))?
        .compile_matcher();
    let mut hits: Vec<(SystemTime, PathBuf)> = Vec::new();
    // require_git(false): honour .gitignore even outside a git checkout.
    for entry in ignore::WalkBuilder::new(root)
        .require_git(false)
        .build()
        .flatten()
    {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap_or(entry.path());
        if glob.is_match(rel) {
            let mtime = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            hits.push((mtime, entry.path().to_path_buf()));
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let truncated = hits.len() > MAX_RESULTS;
    hits.truncate(MAX_RESULTS);
    Ok((hits.into_iter().map(|(_, p)| p).collect(), truncated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testing::*;

    #[tokio::test]
    async fn matches_respects_gitignore_and_orders_by_mtime() {
        let (_d, ctx) = ctx();
        write(&ctx, ".gitignore", "ignored/\n");
        write(&ctx, "ignored/x.rs", "");
        let old = write(&ctx, "src/old.rs", "");
        write(&ctx, "src/other.txt", "");
        std::thread::sleep(std::time::Duration::from_millis(20));
        let new = write(&ctx, "src/deep/new.rs", "");
        // Make the ordering deterministic regardless of filesystem timestamp resolution.
        filetime_touch(&old, 1_000_000);
        filetime_touch(&new, 2_000_000);

        let out = Glob
            .call(
                json!({ "pattern": "**/*.rs" }),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        let lines: Vec<&str> = out.content.lines().collect();
        assert_eq!(lines, vec![new.to_str().unwrap(), old.to_str().unwrap()]);

        let out = Glob
            .call(
                json!({ "pattern": "*.rs", "path": ctx.workdir.join("src") }),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(out.content, old.to_str().unwrap());

        let out = Glob
            .call(json!({ "pattern": "*.py" }), &ctx, CancellationToken::new())
            .await;
        assert_eq!(out.content, "No files found");
    }

    #[tokio::test]
    async fn caps_results() {
        let (_d, ctx) = ctx();
        for i in 0..(MAX_RESULTS + 5) {
            write(&ctx, &format!("f{i}.txt"), "");
        }
        let out = Glob
            .call(
                json!({ "pattern": "*.txt" }),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(out.content.lines().count(), MAX_RESULTS + 1);
        assert!(out.content.ends_with("more specific path or pattern.)"));
        assert_eq!(out.meta["truncated"], true);
    }

    fn filetime_touch(p: &Path, secs: u64) {
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        let f = std::fs::File::options().write(true).open(p).unwrap();
        f.set_modified(t).unwrap();
    }
}
