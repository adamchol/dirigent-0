//! Claude Code style tools: `Bash`, `Read`, `Write`, `Edit`, `Glob`, `Grep`.
//!
//! Same names, parameters, output formats and error strings Claude Code uses,
//! because that is what current Claude models are most fluent with. They are
//! plain JSON-schema function tools, so they work through any provider.
//!
//! The working directory is the sandbox: every path must be absolute and
//! resolve inside it, and `Bash` runs there.

pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod write;

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::provider::ToolSpec;

#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    pub meta: Value,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        ToolOutput {
            content: content.into(),
            is_error: false,
            meta: Value::Null,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        ToolOutput {
            content: content.into(),
            is_error: true,
            meta: Value::Null,
        }
    }

    pub fn with_meta(mut self, meta: Value) -> Self {
        self.meta = meta;
        self
    }
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// Read-only tools in one batch run concurrently; the rest run serially.
    fn read_only(&self) -> bool;
    /// `cancel` fires when the turn is interrupted; long-running tools must stop.
    async fn call(&self, input: Value, ctx: &ToolCtx, cancel: CancellationToken) -> ToolOutput;
}

// ---------------------------------------------------------------------------
// Context: sandbox root + read tracking
// ---------------------------------------------------------------------------

pub const ERR_NOT_READ: &str = "File has not been read yet. Read it first before writing to it.";
pub const ERR_MODIFIED: &str =
    "File has been unexpectedly modified. Read it again before attempting to write it.";
pub const ERR_PARTIAL_READ: &str =
    "File was only partially read. Read the whole file (without offset/limit) before editing it.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadRecord {
    pub mtime: SystemTime,
    pub len: u64,
    pub partial: bool,
}

pub struct ToolCtx {
    pub workdir: &'static Path,
    reads: Mutex<HashMap<PathBuf, ReadRecord>>,
}

impl ToolCtx {
    /// `workdir` must already be canonical.
    pub fn new(workdir: &'static Path) -> Self {
        ToolCtx {
            workdir,
            reads: Mutex::new(HashMap::new()),
        }
    }

    /// Absolute path, `.`/`..` resolved, symlinks in the existing prefix
    /// resolved, and inside the working directory.
    pub fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let p = Path::new(path);
        if !p.is_absolute() {
            return Err(format!("Path must be absolute, got: {path}"));
        }
        let normalized = normalize(p);
        let resolved = canonicalize_prefix(&normalized);
        if !resolved.starts_with(self.workdir) {
            return Err(format!(
                "Path is outside the working directory {}: {path}",
                self.workdir.display()
            ));
        }
        Ok(resolved)
    }

    pub fn record_read(&self, path: &Path, partial: bool) {
        if let Ok(meta) = std::fs::metadata(path) {
            let record = ReadRecord {
                mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                len: meta.len(),
                partial,
            };
            self.reads
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), record);
        }
    }

    /// Claude Code's guard: an existing file may only be modified after a
    /// full read, and only if it has not changed since.
    pub fn check_modifiable(&self, path: &Path) -> Result<(), String> {
        let Ok(meta) = std::fs::metadata(path) else {
            return Ok(()); // new file
        };
        let reads = self.reads.lock().unwrap();
        let Some(rec) = reads.get(path) else {
            return Err(ERR_NOT_READ.into());
        };
        if rec.partial {
            return Err(ERR_PARTIAL_READ.into());
        }
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        if mtime != rec.mtime || meta.len() != rec.len {
            return Err(ERR_MODIFIED.into());
        }
        Ok(())
    }
}

fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Canonicalize the longest existing ancestor and re-append the rest, so a
/// not-yet-created file still resolves through symlinked parents.
fn canonicalize_prefix(p: &Path) -> PathBuf {
    let mut existing = p.to_path_buf();
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    while !existing.exists() {
        match existing.file_name() {
            Some(name) => {
                rest.push(name.to_os_string());
                existing.pop();
            }
            None => break,
        }
    }
    let mut out = existing.canonicalize().unwrap_or(existing);
    for part in rest.iter().rev() {
        out.push(part);
    }
    out
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

pub struct Registry {
    tools: Vec<Arc<dyn Tool>>,
    specs: Vec<ToolSpec>,
}

impl Registry {
    pub fn new(tools: Vec<Arc<dyn Tool>>) -> Self {
        // Built once: the tool array must be byte-stable across calls for caching.
        let specs = tools.iter().map(|t| t.spec()).collect();
        Registry { tools, specs }
    }

    pub fn default_set() -> Self {
        Self::new(vec![
            Arc::new(bash::Bash),
            Arc::new(read::Read),
            Arc::new(write::Write),
            Arc::new(edit::Edit),
            Arc::new(glob::Glob),
            Arc::new(grep::Grep),
        ])
    }

    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.spec().name == name).cloned()
    }
}

// ---------------------------------------------------------------------------
// Input helpers
// ---------------------------------------------------------------------------

pub fn req_str<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Missing required parameter: {key}"))
}

pub fn opt_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(Value::as_str)
}

pub fn opt_u64(input: &Value, key: &str) -> Option<u64> {
    input.get(key).and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_f64().map(|f| f.max(0.0) as u64))
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

pub fn opt_bool(input: &Value, key: &str) -> Option<bool> {
    input.get(key).and_then(|v| {
        v.as_bool()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    })
}

/// Keep the first `head` and the last `max - head` chars of `s`.
pub fn truncate_middle(s: &str, max: usize, head: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_string(), false);
    }
    let tail = max.saturating_sub(head);
    let head_end = floor_char_boundary(s, head);
    let tail_start = floor_char_boundary(s, s.len().saturating_sub(tail));
    let cut = tail_start.saturating_sub(head_end);
    (
        format!(
            "{}\n\n[... {cut} chars truncated ...]\n\n{}",
            &s[..head_end],
            &s[tail_start..]
        ),
        true,
    )
}

pub fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// `cat -n` style numbering, `start` is the 1-based number of the first line.
pub fn format_lines<'a>(lines: impl IntoIterator<Item = &'a str>, start: usize) -> String {
    let mut out = String::new();
    for (i, line) in lines.into_iter().enumerate() {
        out.push_str(&format!("{:>6}\t{}\n", start + i, line));
    }
    out
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// A canonical, leaked tempdir plus a context rooted at it.
    pub fn ctx() -> (tempfile::TempDir, ToolCtx) {
        let dir = tempfile::tempdir().unwrap();
        let root: &'static Path = Box::leak(dir.path().canonicalize().unwrap().into_boxed_path());
        (dir, ToolCtx::new(root))
    }

    pub fn write(ctx: &ToolCtx, rel: &str, content: &str) -> PathBuf {
        let p = ctx.workdir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, content).unwrap();
        p
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn resolve_rejects_relative_and_escapes() {
        let (_d, ctx) = ctx();
        assert!(ctx.resolve("src/main.rs").unwrap_err().contains("absolute"));
        let outside = format!("{}/../../etc/passwd", ctx.workdir.display());
        assert!(ctx.resolve(&outside).unwrap_err().contains("outside"));
        assert!(ctx.resolve("/etc/passwd").unwrap_err().contains("outside"));
        // Not-yet-existing file under workdir is fine, and `.`/`..` inside are resolved.
        let inside = format!("{}/a/./b/../new.txt", ctx.workdir.display());
        assert_eq!(ctx.resolve(&inside).unwrap(), ctx.workdir.join("a/new.txt"));
    }

    #[test]
    fn modify_guard() {
        let (_d, ctx) = ctx();
        let p = write(&ctx, "f.txt", "one");
        assert_eq!(ctx.check_modifiable(&p).unwrap_err(), ERR_NOT_READ);
        ctx.record_read(&p, true);
        assert_eq!(ctx.check_modifiable(&p).unwrap_err(), ERR_PARTIAL_READ);
        ctx.record_read(&p, false);
        assert!(ctx.check_modifiable(&p).is_ok());
        std::fs::write(&p, "changed!").unwrap();
        assert_eq!(ctx.check_modifiable(&p).unwrap_err(), ERR_MODIFIED);
        // New files never need a read.
        assert!(ctx.check_modifiable(&ctx.workdir.join("nope.txt")).is_ok());
    }

    #[test]
    fn truncate_middle_keeps_head_and_tail() {
        let s = "a".repeat(50) + &"b".repeat(50);
        let (t, cut) = truncate_middle(&s, 30, 20);
        assert!(cut);
        assert!(t.starts_with(&"a".repeat(20)));
        assert!(t.ends_with(&"b".repeat(10)));
        assert!(t.contains("[... 70 chars truncated ...]"));
        assert_eq!(truncate_middle("short", 30, 20), ("short".into(), false));
    }

    #[test]
    fn format_lines_is_cat_n() {
        assert_eq!(format_lines(["a", "b"], 9), "     9\ta\n    10\tb\n");
    }
}
