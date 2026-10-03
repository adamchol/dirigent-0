//! `Grep`, ripgrep semantics via ripgrep's own library crates (no `rg` binary).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{opt_bool, opt_str, opt_u64, req_str, Tool, ToolCtx, ToolOutput};
use crate::provider::ToolSpec;

pub const DEFAULT_HEAD_LIMIT: usize = 250;
pub const MAX_LINE_CHARS: usize = 500;

pub struct Grep;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    #[default]
    Files,
    Content,
    Count,
}

#[async_trait]
impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "Grep".into(),
            description: "A powerful search tool built on ripgrep. Always use this tool for searching file contents instead of grep or rg via Bash. Supports full regex syntax (e.g. \"log.*Error\", \"function\\s+\\w+\"). Filter files with the glob parameter (e.g. \"*.rs\", \"**/*.{ts,tsx}\") or the type parameter (e.g. \"rust\", \"py\", \"js\"). output_mode: \"files_with_matches\" (default) lists file paths, \"content\" shows matching lines with line numbers, \"count\" shows match counts per file. Use -A/-B/-C for context lines in content mode. Literal braces need escaping (use \"interface\\{\\}\" to find \"interface{}\"). Respects .gitignore.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "The regular expression pattern to search for in file contents" },
                    "path": { "type": "string", "description": "File or directory to search in (absolute path). Defaults to the working directory." },
                    "glob": { "type": "string", "description": "Glob pattern to filter files (e.g. \"*.js\", \"*.{ts,tsx}\")" },
                    "type": { "type": "string", "description": "File type to search (e.g. rust, py, js, ts, go, md, json, yaml, toml, nix, sh)" },
                    "output_mode": { "type": "string", "enum": ["content", "files_with_matches", "count"], "description": "Output mode. Defaults to \"files_with_matches\"." },
                    "-i": { "type": "boolean", "description": "Case insensitive search" },
                    "-n": { "type": "boolean", "description": "Show line numbers in content mode (default true)" },
                    "-A": { "type": "number", "description": "Lines to show after each match (content mode)" },
                    "-B": { "type": "number", "description": "Lines to show before each match (content mode)" },
                    "-C": { "type": "number", "description": "Lines to show before and after each match (content mode)" },
                    "head_limit": { "type": "number", "description": "Limit output to first N lines/entries (default 250)" },
                    "multiline": { "type": "boolean", "description": "Enable multiline mode where . matches newlines and patterns can span lines (default false)" }
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
        if !root.exists() {
            return ToolOutput::error(format!("Path does not exist: {}", root.display()));
        }
        let mode = match opt_str(&input, "output_mode").unwrap_or("files_with_matches") {
            "content" => Mode::Content,
            "count" => Mode::Count,
            "files_with_matches" => Mode::Files,
            other => return ToolOutput::error(format!("Unknown output_mode: {other}")),
        };
        let opts = Options {
            glob: opt_str(&input, "glob").map(String::from),
            file_type: opt_str(&input, "type").map(String::from),
            case_insensitive: opt_bool(&input, "-i").unwrap_or(false),
            line_numbers: opt_bool(&input, "-n").unwrap_or(true),
            before: opt_u64(&input, "-B")
                .or_else(|| opt_u64(&input, "-C"))
                .unwrap_or(0) as usize,
            after: opt_u64(&input, "-A")
                .or_else(|| opt_u64(&input, "-C"))
                .unwrap_or(0) as usize,
            head_limit: opt_u64(&input, "head_limit")
                .map(|n| n as usize)
                .unwrap_or(DEFAULT_HEAD_LIMIT)
                .max(1),
            multiline: opt_bool(&input, "multiline").unwrap_or(false),
        };
        let result =
            tokio::task::spawn_blocking(move || search(&root, &pattern, mode, &opts)).await;
        match result {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => ToolOutput::error(e),
            Err(e) => ToolOutput::error(format!("Grep failed: {e}")),
        }
    }
}

struct Options {
    glob: Option<String>,
    file_type: Option<String>,
    case_insensitive: bool,
    line_numbers: bool,
    before: usize,
    after: usize,
    head_limit: usize,
    multiline: bool,
}

/// Extensions for the `type` parameter (subset of ripgrep's type list).
fn type_globs(t: &str) -> Option<&'static [&'static str]> {
    Some(match t {
        "rust" | "rs" => &["*.rs"],
        "py" | "python" => &["*.py", "*.pyi"],
        "js" => &["*.js", "*.jsx", "*.mjs", "*.cjs"],
        "ts" => &["*.ts", "*.tsx", "*.mts", "*.cts"],
        "go" => &["*.go"],
        "md" | "markdown" => &["*.md", "*.markdown"],
        "json" => &["*.json"],
        "yaml" | "yml" => &["*.yaml", "*.yml"],
        "toml" => &["*.toml"],
        "nix" => &["*.nix"],
        "sh" | "bash" => &["*.sh", "*.bash", "*.zsh"],
        "html" => &["*.html", "*.htm"],
        "css" => &["*.css", "*.scss"],
        "c" => &["*.c", "*.h"],
        "cpp" => &["*.cpp", "*.cc", "*.cxx", "*.hpp", "*.hh"],
        "java" => &["*.java"],
        "kotlin" | "kt" => &["*.kt", "*.kts"],
        "swift" => &["*.swift"],
        "rb" | "ruby" => &["*.rb"],
        "sql" => &["*.sql"],
        "txt" => &["*.txt"],
        _ => return None,
    })
}

fn search(root: &Path, pattern: &str, mode: Mode, opts: &Options) -> Result<ToolOutput, String> {
    let matcher = grep_regex::RegexMatcherBuilder::new()
        .case_insensitive(opts.case_insensitive)
        .multi_line(opts.multiline)
        .dot_matches_new_line(opts.multiline)
        .build(pattern)
        .map_err(|e| format!("Invalid regex: {e}"))?;

    let mut globs = globset::GlobSetBuilder::new();
    let mut has_filter = false;
    if let Some(g) = &opts.glob {
        globs.add(globset::Glob::new(g).map_err(|e| format!("Invalid glob: {e}"))?);
        has_filter = true;
    }
    if let Some(t) = &opts.file_type {
        let Some(patterns) = type_globs(t) else {
            return Err(format!("Unknown file type: {t}"));
        };
        for p in patterns {
            globs.add(globset::Glob::new(p).expect("static glob"));
        }
        has_filter = true;
    }
    let globs = globs.build().map_err(|e| e.to_string())?;

    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .multi_line(opts.multiline)
        .before_context(if mode == Mode::Content {
            opts.before
        } else {
            0
        })
        .after_context(if mode == Mode::Content { opts.after } else { 0 })
        .build();

    let mut files: Vec<(SystemTime, PathBuf, FileResult)> = Vec::new();
    let mut total_lines = 0usize;
    let limit_hit = |total: usize| mode == Mode::Content && total >= opts.head_limit;

    for entry in ignore::WalkBuilder::new(root)
        .require_git(false)
        .build()
        .flatten()
    {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if has_filter {
            let name = path.file_name().map(Path::new).unwrap_or(path);
            let rel = path.strip_prefix(root).unwrap_or(path);
            if !globs.is_match(name) && !globs.is_match(rel) {
                continue;
            }
        }
        let mut sink = Collector {
            mode,
            path: path.to_path_buf(),
            line_numbers: opts.line_numbers,
            budget: opts.head_limit.saturating_sub(total_lines),
            ..Default::default()
        };
        if let Err(e) = searcher.search_path(&matcher, path, &mut sink) {
            tracing::debug!(path = %path.display(), "grep skipped: {e}");
            continue;
        }
        if sink.matches == 0 {
            continue;
        }
        total_lines += sink.lines.len();
        let mtime = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        files.push((
            mtime,
            path.to_path_buf(),
            FileResult {
                matches: sink.matches,
                lines: sink.lines,
                truncated: sink.truncated,
            },
        ));
        if limit_hit(total_lines) {
            break;
        }
    }

    if files.is_empty() {
        return Ok(ToolOutput::ok("No matches found"));
    }

    let (body, truncated) = match mode {
        Mode::Files => {
            files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            let truncated = files.len() > opts.head_limit;
            files.truncate(opts.head_limit);
            let lines: Vec<String> = files.iter().map(|f| f.1.display().to_string()).collect();
            (lines.join("\n"), truncated)
        }
        Mode::Count => {
            let truncated = files.len() > opts.head_limit;
            files.truncate(opts.head_limit);
            let lines: Vec<String> = files
                .iter()
                .map(|f| format!("{}:{}", f.1.display(), f.2.matches))
                .collect();
            (lines.join("\n"), truncated)
        }
        Mode::Content => {
            let truncated = files.iter().any(|f| f.2.truncated) || total_lines >= opts.head_limit;
            let mut out: Vec<String> = Vec::new();
            for (i, f) in files.iter().enumerate() {
                if i > 0 && (opts.before > 0 || opts.after > 0) {
                    out.push("--".into());
                }
                out.extend(f.2.lines.iter().cloned());
            }
            (out.join("\n"), truncated)
        }
    };
    let mut out = body;
    if truncated {
        out.push_str(&format!(
            "\n[Output truncated to {} entries. Narrow the search with path, glob or a more specific pattern.]",
            opts.head_limit
        ));
    }
    let count = match mode {
        Mode::Content => total_lines,
        _ => files.len(),
    };
    Ok(ToolOutput::ok(out)
        .with_meta(json!({ "files": files.len(), "entries": count, "truncated": truncated })))
}

struct FileResult {
    matches: usize,
    lines: Vec<String>,
    truncated: bool,
}

#[derive(Default)]
struct Collector {
    mode: Mode,
    path: PathBuf,
    line_numbers: bool,
    budget: usize,
    matches: usize,
    lines: Vec<String>,
    truncated: bool,
}

impl Collector {
    fn push(&mut self, line_number: Option<u64>, sep: char, bytes: &[u8]) -> bool {
        if self.lines.len() >= self.budget {
            self.truncated = true;
            return false;
        }
        let text = String::from_utf8_lossy(bytes);
        let text = text.trim_end_matches(['\n', '\r']);
        let text: String = if text.chars().count() > MAX_LINE_CHARS {
            format!("{}…", text.chars().take(MAX_LINE_CHARS).collect::<String>())
        } else {
            text.to_string()
        };
        let prefix = match (self.line_numbers, line_number) {
            (true, Some(n)) => format!("{}{sep}{n}{sep}", self.path.display()),
            _ => format!("{}{sep}", self.path.display()),
        };
        self.lines.push(format!("{prefix}{text}"));
        true
    }
}

impl Sink for Collector {
    type Error = std::io::Error;

    fn matched(&mut self, _s: &Searcher, m: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        self.matches += 1;
        match self.mode {
            Mode::Files => Ok(false), // one hit is enough
            Mode::Count => Ok(true),
            Mode::Content => Ok(self.push(m.line_number(), ':', m.bytes())),
        }
    }

    fn context(&mut self, _s: &Searcher, c: &SinkContext<'_>) -> Result<bool, Self::Error> {
        if self.mode != Mode::Content {
            return Ok(true);
        }
        Ok(self.push(c.line_number(), '-', c.bytes()))
    }

    fn context_break(&mut self, _s: &Searcher) -> Result<bool, Self::Error> {
        if self.mode == Mode::Content && !self.lines.is_empty() && self.lines.len() < self.budget {
            self.lines.push("--".into());
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testing::*;

    fn setup() -> (tempfile::TempDir, ToolCtx) {
        let (d, ctx) = ctx();
        write(
            &ctx,
            "src/a.rs",
            "fn main() {}\nlet Error = 1;\n// error here\n",
        );
        write(&ctx, "src/b.py", "print('error')\n");
        write(&ctx, "notes.md", "no hits\n");
        write(&ctx, ".gitignore", "ignored.rs\n");
        write(&ctx, "ignored.rs", "error\n");
        (d, ctx)
    }

    async fn grep(ctx: &ToolCtx, input: Value) -> ToolOutput {
        Grep.call(input, ctx, CancellationToken::new()).await
    }

    #[tokio::test]
    async fn files_mode_default_respects_gitignore_and_filters() {
        let (_d, ctx) = setup();
        let out = grep(&ctx, json!({ "pattern": "error" })).await;
        let mut lines: Vec<&str> = out.content.lines().collect();
        lines.sort();
        assert_eq!(
            lines,
            vec![
                ctx.workdir.join("src/a.rs").to_str().unwrap(),
                ctx.workdir.join("src/b.py").to_str().unwrap()
            ]
        );
        let out = grep(&ctx, json!({ "pattern": "error", "type": "rust" })).await;
        assert_eq!(out.content, ctx.workdir.join("src/a.rs").to_str().unwrap());
        let out = grep(&ctx, json!({ "pattern": "error", "glob": "*.py" })).await;
        assert_eq!(out.content, ctx.workdir.join("src/b.py").to_str().unwrap());
        let out = grep(&ctx, json!({ "pattern": "nothing_here" })).await;
        assert_eq!(out.content, "No matches found");
    }

    #[tokio::test]
    async fn content_count_and_context() {
        let (_d, ctx) = setup();
        let a = ctx.workdir.join("src/a.rs");
        let out = grep(
            &ctx,
            json!({ "pattern": "error", "output_mode": "content", "path": a, "-i": true }),
        )
        .await;
        assert_eq!(
            out.content,
            format!("{a}:2:let Error = 1;\n{a}:3:// error here", a = a.display())
        );
        let out = grep(
            &ctx,
            json!({ "pattern": "Error", "output_mode": "content", "path": a, "-B": 1 }),
        )
        .await;
        assert_eq!(
            out.content,
            format!(
                "{a}-1-fn main() {{}}\n{a}:2:let Error = 1;",
                a = a.display()
            )
        );
        let out = grep(
            &ctx,
            json!({ "pattern": "error", "output_mode": "count", "path": a, "-i": true }),
        )
        .await;
        assert_eq!(out.content, format!("{}:2", a.display()));
        let out = grep(
            &ctx,
            json!({ "pattern": "error", "output_mode": "content", "-i": true, "head_limit": 1 }),
        )
        .await;
        assert_eq!(out.content.lines().count(), 2);
        assert!(out.content.contains("[Output truncated to 1 entries."));
        let out = grep(&ctx, json!({ "pattern": "(" })).await;
        assert!(out.is_error && out.content.starts_with("Invalid regex"));
    }
}
