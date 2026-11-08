//! Which files of a workspace a commit has, and their blobs: kopia's checkpoints' rules (only
//! `.gitignore`s leave files out, `.git` folders aside, and files over [`MAX_FILE_SIZE`]),
//! hashed again only when their stat says they changed.

use std::collections::HashMap;
use std::fs::Metadata;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;

use crate::Repo;

/// Bigger files are left out (as checkpoints' `--max-file-size` does).
pub const MAX_FILE_SIZE: u64 = 50_000_000;
/// A file changed this recently is hashed again: a change within the same tick of its clock
/// wouldn't show in its stat.
const RACY: Duration = Duration::from_secs(2);

const FILE: u32 = 0o100644;
const EXECUTABLE: u32 = 0o100755;
const LINK: u32 = 0o120000;

/// A file's stat, as far as it tells whether the file changed.
#[derive(Clone, Copy, PartialEq)]
struct Stat {
    mtime: i128,
    ctime: i128,
    size: u64,
    ino: u64,
    mode: u32,
}

impl Stat {
    fn of(meta: &Metadata) -> Stat {
        let mode = if meta.file_type().is_symlink() {
            LINK
        } else if meta.mode() & 0o111 != 0 {
            EXECUTABLE
        } else {
            FILE
        };
        Stat {
            mtime: meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128,
            ctime: meta.ctime() as i128 * 1_000_000_000 + meta.ctime_nsec() as i128,
            size: meta.len(),
            ino: meta.ino(),
            mode,
        }
    }
}

/// A workspace's files, as last read: each one's stat and blob.
pub struct Workspace {
    root: PathBuf,
    seen: HashMap<PathBuf, (Stat, String)>,
    /// Values replaced in the files' blobs by `[secret:NAME]`, as `(NAME, value)`.
    redact: Vec<(String, String)>,
}

impl Workspace {
    pub fn new(root: &Path) -> Workspace {
        Workspace {
            root: root.to_path_buf(),
            seen: HashMap::new(),
            redact: Vec::new(),
        }
    }

    /// Replace each value in the files' blobs by `[secret:NAME]` from now on (`(NAME, value)`):
    /// files are hashed again when these change, since their stat says nothing of it.
    pub fn redact(&mut self, redact: Vec<(String, String)>) {
        if redact != self.redact {
            self.redact = redact;
            self.seen.clear();
        }
    }

    /// The files a commit has, relative to the root, sorted.
    pub async fn list(&self) -> anyhow::Result<Vec<PathBuf>> {
        Ok(self.walk().await?.into_iter().map(|(p, _)| p).collect())
    }

    /// Every file a commit has, as `(mode, blob, path)`, its blob written to `repo`.
    pub(crate) async fn read(
        &mut self,
        repo: &Repo,
    ) -> anyhow::Result<Vec<(u32, String, PathBuf)>> {
        let racy = (SystemTime::now() - RACY)
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as i128);
        let found = self.walk().await?;
        let mut files = Vec::new();
        let mut links = Vec::new();
        let mut secret = Vec::new();
        let mut seen = HashMap::with_capacity(found.len());
        for (path, meta) in found {
            let stat = Stat::of(&meta);
            match self.seen.remove(&path) {
                Some((was, blob)) if was == stat && stat.mtime < racy => {
                    seen.insert(path, (stat, blob));
                }
                _ if stat.mode == LINK => links.push((path, stat)),
                // one with a secret in it, redacted
                _ if self.has_secret(&path) => secret.push((path, stat)),
                // a path git can't take one per line
                _ if path.as_os_str().as_bytes().contains(&b'\n') => {}
                _ => files.push((path, stat)),
            }
        }
        if !files.is_empty() {
            let mut list = Vec::new();
            for (path, _) in &files {
                list.extend_from_slice(path.as_os_str().as_bytes());
                list.push(b'\n');
            }
            let mut cmd = repo.git();
            cmd.current_dir(&self.root).args([
                "hash-object",
                "-w",
                "--no-filters",
                "--stdin-paths",
            ]);
            let out = repo
                .input(&mut cmd, list)
                .await
                .context("hashing the workspace's files")?;
            let blobs = String::from_utf8_lossy(&out);
            let blobs: Vec<&str> = blobs.lines().collect();
            anyhow::ensure!(
                blobs.len() == files.len(),
                "git hashed {} of {} files",
                blobs.len(),
                files.len()
            );
            for ((path, stat), blob) in files.into_iter().zip(blobs) {
                seen.insert(path, (stat, blob.to_string()));
            }
        }
        for (path, stat) in secret {
            let Ok(text) = std::fs::read(self.root.join(&path)) else {
                continue;
            };
            let mut cmd = repo.git();
            cmd.args(["hash-object", "-w", "--stdin"]);
            let out = repo.input(&mut cmd, self.redacted(text)).await?;
            seen.insert(
                path,
                (stat, String::from_utf8_lossy(&out).trim().to_string()),
            );
        }
        for (path, stat) in links {
            let Ok(target) = std::fs::read_link(self.root.join(&path)) else {
                continue;
            };
            let mut cmd = repo.git();
            cmd.args(["hash-object", "-w", "--stdin"]);
            let out = repo
                .input(&mut cmd, target.as_os_str().as_bytes().to_vec())
                .await?;
            seen.insert(
                path,
                (stat, String::from_utf8_lossy(&out).trim().to_string()),
            );
        }
        self.seen = seen;
        let mut entries: Vec<_> = self
            .seen
            .iter()
            .map(|(path, (stat, blob))| (stat.mode, blob.clone(), path.clone()))
            .collect();
        entries.sort_by(|a, b| a.2.cmp(&b.2));
        Ok(entries)
    }

    /// Whether the file at `path` has a value to redact in it.
    fn has_secret(&self, path: &Path) -> bool {
        if self.redact.is_empty() {
            return false;
        }
        let Ok(text) = std::fs::read(self.root.join(path)) else {
            return false;
        };
        self.redact
            .iter()
            .any(|(_, v)| contains(&text, v.as_bytes()))
    }

    /// `text` with each value replaced by `[secret:NAME]`, the longest first.
    fn redacted(&self, mut text: Vec<u8>) -> Vec<u8> {
        let mut values: Vec<&(String, String)> = self.redact.iter().collect();
        values.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
        for (name, value) in values {
            text = replace(
                &text,
                value.as_bytes(),
                format!("[secret:{name}]").as_bytes(),
            );
        }
        text
    }

    /// The files under the root a commit has, with their stat.
    async fn walk(&self) -> anyhow::Result<Vec<(PathBuf, Metadata)>> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || walk(&root))
            .await
            .context("walking the workspace")?
    }
}

fn walk(root: &Path) -> anyhow::Result<Vec<(PathBuf, Metadata)>> {
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .parents(false)
        .ignore(false)
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
        .follow_links(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build();
    let mut found = Vec::new();
    for entry in walker {
        // unreadable: left out, as it can't be read anyway
        let Ok(entry) = entry else {
            continue;
        };
        let Some(kind) = entry.file_type() else {
            continue;
        };
        if !(kind.is_file() || kind.is_symlink()) {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            // gone since it was listed
            Err(_) => continue,
        };
        if kind.is_file() && meta.len() > MAX_FILE_SIZE {
            continue;
        }
        let Ok(path) = entry.path().strip_prefix(root) else {
            continue;
        };
        found.push((path.to_path_buf(), meta));
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

fn contains(text: &[u8], value: &[u8]) -> bool {
    !value.is_empty() && text.windows(value.len()).any(|w| w == value)
}

/// `text` with every `from` replaced by `to`.
pub fn replace(text: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() {
        return text.to_vec();
    }
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(text[i]);
            i += 1;
        }
    }
    out
}
