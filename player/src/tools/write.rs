use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{req_str, Tool, ToolCtx, ToolOutput};
use crate::provider::ToolSpec;

pub struct Write;

#[async_trait]
impl Tool for Write {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Write".into(),
            description: "Writes a file to the local filesystem, creating parent directories as needed. This tool overwrites the existing file if there is one at the provided path. If this is an existing file, you MUST use the Read tool first to read the file's contents; the write fails otherwise. Prefer editing existing files with Edit over rewriting them. The file_path must be absolute.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "The absolute path to the file to write (must be absolute, not relative)" },
                    "content": { "type": "string", "description": "The content to write to the file" }
                },
                "required": ["file_path", "content"],
                "additionalProperties": false
            }),
        }
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn call(&self, input: Value, ctx: &ToolCtx, _cancel: CancellationToken) -> ToolOutput {
        let path = match req_str(&input, "file_path").and_then(|p| ctx.resolve(p)) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        let content = match req_str(&input, "content") {
            Ok(c) => c,
            Err(e) => return ToolOutput::error(e),
        };
        if path.is_dir() {
            return ToolOutput::error("EISDIR: illegal operation on a directory, write");
        }
        let existed = path.exists();
        if let Err(e) = ctx.check_modifiable(&path) {
            return ToolOutput::error(e);
        }
        if let Some(parent) = path.parent() {
            if let Err(e) = tokio::fs::create_dir_all(parent).await {
                return ToolOutput::error(format!("Failed to create directory: {e}"));
            }
        }
        if let Err(e) = tokio::fs::write(&path, content).await {
            return ToolOutput::error(format!("Failed to write file: {e}"));
        }
        ctx.record_read(&path);
        let msg = if existed {
            format!("The file {} has been updated successfully.", path.display())
        } else {
            format!("File created successfully at: {}", path.display())
        };
        ToolOutput::ok(msg).with_meta(json!({ "created": !existed, "bytes": content.len() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testing::*;
    use crate::tools::{ERR_MODIFIED, ERR_NOT_READ};

    #[tokio::test]
    async fn creates_nested_file_and_guards_existing() {
        let (_d, ctx) = ctx();
        let p = ctx.workdir.join("a/b/new.txt");
        let out = Write
            .call(
                json!({ "file_path": p, "content": "hi" }),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.starts_with("File created successfully at: "));
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "hi");

        // Just written by us: counts as read, so a second write is fine.
        let out = Write
            .call(
                json!({ "file_path": p, "content": "hi2" }),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert!(out.content.ends_with("has been updated successfully."));

        // Existing file this session never read -> guard.
        let q = write(&ctx, "old.txt", "x");
        let out = Write
            .call(
                json!({ "file_path": q, "content": "y" }),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(out.content, ERR_NOT_READ);
        assert!(out.is_error);

        // Modified behind our back after the read.
        ctx.record_read(&q);
        std::fs::write(&q, "changed content").unwrap();
        let out = Write
            .call(
                json!({ "file_path": q, "content": "y" }),
                &ctx,
                CancellationToken::new(),
            )
            .await;
        assert_eq!(out.content, ERR_MODIFIED);
    }
}
