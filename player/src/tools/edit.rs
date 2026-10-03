use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{format_lines, opt_bool, req_str, Tool, ToolCtx, ToolOutput};
use crate::provider::ToolSpec;

pub const ERR_NOT_FOUND: &str = "String to replace not found in file.";
pub const ERR_SAME: &str = "No changes to make: old_string and new_string are exactly the same.";
const SNIPPET_CONTEXT: usize = 4;

pub struct Edit;

#[async_trait]
impl Tool for Edit {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Edit".into(),
            description: "Performs exact string replacements in files. You must use the Read tool at least once before editing; the edit fails otherwise. The edit fails if old_string is not unique in the file: either provide more surrounding context to make it unique, or set replace_all to true to change every occurrence. Preserve the file's exact indentation (tabs/spaces) as it appears after the line-number tab in Read output; never include the line-number prefix in old_string or new_string. The file_path must be absolute.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "The absolute path to the file to modify" },
                    "old_string": { "type": "string", "description": "The text to replace" },
                    "new_string": { "type": "string", "description": "The text to replace it with (must be different from old_string)" },
                    "replace_all": { "type": "boolean", "default": false, "description": "Replace all occurrences of old_string (default false)" }
                },
                "required": ["file_path", "old_string", "new_string"],
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
        let (old, new) = match (req_str(&input, "old_string"), req_str(&input, "new_string")) {
            (Ok(o), Ok(n)) => (o, n),
            (Err(e), _) | (_, Err(e)) => return ToolOutput::error(e),
        };
        let replace_all = opt_bool(&input, "replace_all").unwrap_or(false);
        if old == new {
            return ToolOutput::error(ERR_SAME);
        }
        if !path.exists() {
            return ToolOutput::error("File does not exist.");
        }
        if let Err(e) = ctx.check_modifiable(&path) {
            return ToolOutput::error(e);
        }
        let text = match tokio::fs::read_to_string(&path).await {
            Ok(t) => t,
            Err(e) => return ToolOutput::error(format!("Failed to read file: {e}")),
        };
        if old.is_empty() {
            return ToolOutput::error("old_string must not be empty.");
        }
        let count = text.matches(old).count();
        if count == 0 {
            return ToolOutput::error(ERR_NOT_FOUND);
        }
        if count > 1 && !replace_all {
            return ToolOutput::error(format!(
                "Found {count} matches of the string to replace, but replace_all is false. To replace all occurrences, set replace_all to true. To replace only one occurrence, please provide more context to uniquely identify the instance."
            ));
        }
        let first = text.find(old).expect("count > 0");
        let updated = if replace_all {
            text.replace(old, new)
        } else {
            text.replacen(old, new, 1)
        };
        if let Err(e) = tokio::fs::write(&path, &updated).await {
            return ToolOutput::error(format!("Failed to write file: {e}"));
        }
        ctx.record_read(&path, false);

        let replacements = if replace_all { count } else { 1 };
        let snippet = snippet_around(&updated, first, new.len());
        ToolOutput::ok(format!(
            "The file {} has been updated successfully. Here's the result of running `cat -n` on a snippet of the edited file:\n{}",
            path.display(),
            snippet
        ))
        .with_meta(json!({ "replacements": replacements }))
    }
}

/// `cat -n` of the lines touched by the change at byte `start..start+len`,
/// plus a few lines of context on each side.
fn snippet_around(text: &str, start: usize, len: usize) -> String {
    let first_line = text[..start].matches('\n').count();
    let last_line = text[..(start + len).min(text.len())].matches('\n').count();
    let lines: Vec<&str> = text.lines().collect();
    let from = first_line.saturating_sub(SNIPPET_CONTEXT);
    let to = (last_line + SNIPPET_CONTEXT + 1).min(lines.len());
    format_lines(lines[from..to].iter().copied(), from + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testing::*;
    use crate::tools::ERR_NOT_READ;

    async fn edit(ctx: &ToolCtx, input: Value) -> ToolOutput {
        Edit.call(input, ctx, CancellationToken::new()).await
    }

    #[tokio::test]
    async fn replaces_unique_match_and_returns_snippet() {
        let (_d, ctx) = ctx();
        let content: String = (1..=12).map(|i| format!("line {i}\n")).collect();
        let p = write(&ctx, "f.txt", &content);
        ctx.record_read(&p, false);
        let out = edit(
            &ctx,
            json!({ "file_path": p, "old_string": "line 7", "new_string": "LINE SEVEN" }),
        )
        .await;
        assert!(!out.is_error, "{}", out.content);
        assert!(std::fs::read_to_string(&p)
            .unwrap()
            .contains("LINE SEVEN\n"));
        assert!(out.content.contains("     7\tLINE SEVEN\n"));
        assert!(out.content.contains("     3\tline 3\n"));
        assert!(out.content.contains("    11\tline 11\n"));
        assert!(!out.content.contains("line 12"));
        assert_eq!(out.meta["replacements"], 1);
        // Our own write refreshed the read record.
        assert!(ctx.check_modifiable(&p).is_ok());
    }

    #[tokio::test]
    async fn error_cases() {
        let (_d, ctx) = ctx();
        let p = write(&ctx, "f.txt", "a b a\n");
        let out = edit(
            &ctx,
            json!({ "file_path": p, "old_string": "a", "new_string": "c" }),
        )
        .await;
        assert_eq!(out.content, ERR_NOT_READ);

        ctx.record_read(&p, false);
        let out = edit(
            &ctx,
            json!({ "file_path": p, "old_string": "a", "new_string": "c" }),
        )
        .await;
        assert!(out
            .content
            .starts_with("Found 2 matches of the string to replace, but replace_all is false."));

        let out = edit(
            &ctx,
            json!({ "file_path": p, "old_string": "zzz", "new_string": "c" }),
        )
        .await;
        assert_eq!(out.content, ERR_NOT_FOUND);

        let out = edit(
            &ctx,
            json!({ "file_path": p, "old_string": "a", "new_string": "a" }),
        )
        .await;
        assert_eq!(out.content, ERR_SAME);

        let out = edit(
            &ctx,
            json!({ "file_path": p, "old_string": "a", "new_string": "c", "replace_all": true }),
        )
        .await;
        assert!(!out.is_error);
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "c b c\n");
        assert_eq!(out.meta["replacements"], 2);
    }
}
