use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{floor_char_boundary, format_lines, opt_u64, req_str, Tool, ToolCtx, ToolOutput};
use crate::provider::ToolSpec;

pub const DEFAULT_LIMIT: usize = 2000;
pub const MAX_LINE_CHARS: usize = 2000;
pub const MAX_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 50 * 1024;

pub struct Read;

#[async_trait]
impl Tool for Read {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Read".into(),
            description: "Reads a file from the local filesystem. The file_path parameter must be an absolute path. By default it reads up to 2000 lines from the beginning of the file; use offset and limit for long files. Lines longer than 2000 characters are truncated, and output stops at 50KB; the result then says which offset to continue from. Results are returned in cat -n format, with line numbers starting at 1. You must read a file before editing or overwriting it. Reading a directory or a binary file fails. Always prefer this tool over cat, head, tail or sed via Bash.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "The absolute path to the file to read" },
                    "offset": { "type": "number", "description": "The line number to start reading from (1-based). Only provide if the file is too large to read at once" },
                    "limit": { "type": "number", "description": "The number of lines to read. Only provide if the file is too large to read at once" }
                },
                "required": ["file_path"],
                "additionalProperties": false
            }),
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn call(&self, input: Value, ctx: &ToolCtx, _cancel: CancellationToken) -> ToolOutput {
        let path = match req_str(&input, "file_path").and_then(|p| ctx.resolve(p)) {
            Ok(p) => p,
            Err(e) => return ToolOutput::error(e),
        };
        let offset = opt_u64(&input, "offset")
            .map(|o| o.max(1) as usize)
            .unwrap_or(1);
        let limit = opt_u64(&input, "limit")
            .map(|l| l as usize)
            .unwrap_or(DEFAULT_LIMIT)
            .max(1);

        let meta = match tokio::fs::metadata(&path).await {
            Ok(m) => m,
            Err(_) => return ToolOutput::error("File does not exist."),
        };
        if meta.is_dir() {
            return ToolOutput::error("EISDIR: illegal operation on a directory, read");
        }
        if meta.len() > MAX_BYTES {
            return ToolOutput::error(format!(
                "File is too large to read ({} bytes, max {} bytes). Use offset and limit, or Grep for the part you need.",
                meta.len(),
                MAX_BYTES
            ));
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(b) => b,
            Err(e) => return ToolOutput::error(format!("Failed to read file: {e}")),
        };
        if bytes.is_empty() {
            ctx.record_read(&path);
            return ToolOutput::ok(
                "<system-reminder>Warning: the file exists but has empty contents</system-reminder>",
            );
        }
        if bytes.iter().take(8192).any(|b| *b == 0) {
            return ToolOutput::error("Cannot read binary file.");
        }
        let text = String::from_utf8_lossy(&bytes);
        let all: Vec<&str> = text.lines().collect();
        let total = all.len();
        if offset > total {
            return ToolOutput::error(format!(
                "Offset {offset} is beyond the end of the file ({total} lines)."
            ));
        }
        let end = (offset - 1 + limit).min(total);
        let mut shown: Vec<String> = Vec::new();
        let mut size = 0;
        for l in &all[offset - 1..end] {
            let line = if l.chars().count() > MAX_LINE_CHARS {
                let cut = floor_char_boundary(l, MAX_LINE_CHARS);
                format!("{}…", &l[..cut])
            } else {
                l.to_string()
            };
            // Line number, tab and newline.
            size += line.len() + 8;
            if size > MAX_OUTPUT_BYTES && !shown.is_empty() {
                break;
            }
            shown.push(line);
        }
        let end = offset - 1 + shown.len();
        let mut out = format_lines(shown.iter().map(String::as_str), offset);
        if offset > 1 || end < total {
            out.push_str(&format!(
                "\n[Showing lines {}-{} of {}. Use offset={} to continue.]",
                offset,
                end,
                total,
                end + 1
            ));
        }
        ctx.record_read(&path);
        ToolOutput::ok(out).with_meta(json!({ "lines": total, "from": offset, "to": end }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testing::*;

    async fn read(ctx: &ToolCtx, input: Value) -> ToolOutput {
        Read.call(input, ctx, CancellationToken::new()).await
    }

    #[tokio::test]
    async fn numbers_lines_and_records_read() {
        let (_d, ctx) = ctx();
        let p = write(&ctx, "a.txt", "one\ntwo\nthree\n");
        let out = read(&ctx, json!({ "file_path": p })).await;
        assert!(!out.is_error);
        assert_eq!(out.content, "     1\tone\n     2\ttwo\n     3\tthree\n");
        assert!(ctx.check_modifiable(&p).is_ok());
    }

    #[tokio::test]
    async fn offset_limit_marks_partial() {
        let (_d, ctx) = ctx();
        let content: String = (1..=10).map(|i| format!("l{i}\n")).collect();
        let p = write(&ctx, "a.txt", &content);
        let out = read(&ctx, json!({ "file_path": p, "offset": 4, "limit": 2 })).await;
        assert_eq!(
            out.content,
            "     4\tl4\n     5\tl5\n\n[Showing lines 4-5 of 10. Use offset=6 to continue.]"
        );
        // A partial read is enough to edit.
        assert!(ctx.check_modifiable(&p).is_ok());
        let out = read(&ctx, json!({ "file_path": p, "offset": 11 })).await;
        assert!(out.is_error && out.content.contains("beyond the end"));
    }

    #[tokio::test]
    async fn output_stops_at_a_line_under_the_byte_cap() {
        let (_d, ctx) = ctx();
        let content: String = (0..100).map(|_| "y".repeat(1500) + "\n").collect();
        let p = write(&ctx, "big.txt", &content);
        let out = read(&ctx, json!({ "file_path": p })).await;
        assert!(!out.is_error);
        assert!(out.content.len() <= MAX_OUTPUT_BYTES + 100);
        // 33 lines of 1508 bytes fit in 50KB.
        assert!(out
            .content
            .ends_with("\n[Showing lines 1-33 of 100. Use offset=34 to continue.]"));
        assert!(out.content.contains("    33\t") && !out.content.contains("    34\t"));
        assert_eq!(out.meta["to"], 33);
    }

    #[tokio::test]
    async fn long_lines_are_cut() {
        let (_d, ctx) = ctx();
        let p = write(&ctx, "a.txt", &("x".repeat(2500) + "\nok\n"));
        let out = read(&ctx, json!({ "file_path": p })).await;
        let first = out.content.lines().next().unwrap();
        assert!(first.ends_with('…'));
        assert_eq!(first.chars().count(), 6 + 1 + 2000 + 1);
    }

    #[tokio::test]
    async fn empty_dir_binary_missing() {
        let (_d, ctx) = ctx();
        let p = write(&ctx, "empty.txt", "");
        let out = read(&ctx, json!({ "file_path": p })).await;
        assert!(out.content.contains("empty contents"));
        assert!(ctx.check_modifiable(&p).is_ok());

        let out = read(&ctx, json!({ "file_path": ctx.workdir })).await;
        assert!(out.is_error && out.content.starts_with("EISDIR"));

        let p = ctx.workdir.join("bin");
        std::fs::write(&p, b"abc\0def").unwrap();
        let out = read(&ctx, json!({ "file_path": p })).await;
        assert!(out.is_error && out.content.contains("binary"));

        let out = read(&ctx, json!({ "file_path": ctx.workdir.join("nope") })).await;
        assert_eq!(out.content, "File does not exist.");

        let out = read(&ctx, json!({ "file_path": "relative.txt" })).await;
        assert!(out.is_error && out.content.contains("absolute"));
    }
}
