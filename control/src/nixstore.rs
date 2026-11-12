//! The players' shared Nix store (the chart's `nix.shared`): on a volume every players' VM has
//! (read-only, over virtiofs), generations of a store, each under every VM's own layer of its
//! store (an overlay: nix/vm/base.nix). Players tell what their VM added (`POST /nix/paths`);
//! now and then, control has the next generation written (a Job: the last one's paths, hard
//! linked, and those told, from upstream, or from the players' cache with `promote: all`), and
//! VMs started after get it. A generation is never changed once written, and is deleted once
//! it's neither among the newest nor any VM's. Control has the volume too, at `--nix-store`:
//! `gen-<n>/` (with `complete` once written), and `gen-<n>.todo`, what's to be added to it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::Context;
use tracing::{error, info, warn};

use crate::runner::Kube;
use crate::Control;

/// The generation VMs started now get (0: none yet).
static CURRENT: AtomicU64 = AtomicU64::new(0);

pub fn current() -> Option<u64> {
    Some(CURRENT.load(Ordering::Relaxed)).filter(|n| *n > 0)
}

#[derive(clap::Args, Debug)]
pub struct NixStoreArgs {
    /// The shared store's volume, where its generations are (with KubeVirt players, and
    /// `--kube-writer-template`); without, there's none.
    #[arg(long = "nix-store", env = "DIRIGENT_NIX_STORE")]
    pub dir: Option<PathBuf>,
    /// What a generation takes of what players' VMs added: `upstream`, the paths the upstream
    /// cache signed; `all`, every one (from the players' cache, where VMs put them).
    #[arg(long, env = "DIRIGENT_NIX_PROMOTE", value_enum, default_value_t = Promote::Upstream)]
    pub nix_promote: Promote,
    /// How often a generation is written, at most, in seconds.
    #[arg(long, env = "DIRIGENT_NIX_EVERY", default_value_t = 3600)]
    pub nix_every: u64,
    /// How many generations are kept, besides those VMs have.
    #[arg(long, env = "DIRIGENT_NIX_KEEP", default_value_t = 3)]
    pub nix_keep: usize,
}

#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Promote {
    Upstream,
    All,
}

/// A path a player's VM added, and whether upstream signed it.
#[derive(Debug, serde::Deserialize)]
pub struct Added {
    pub path: String,
    pub signed: bool,
}

/// The generations written in `dir`, oldest first.
fn generations(dir: &Path) -> anyhow::Result<Vec<u64>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if let Some(n) = name.strip_prefix("gen-").and_then(|n| n.parse().ok()) {
            if dir.join(&name).join("complete").exists() {
                found.push(n);
            }
        }
    }
    found.sort();
    Ok(found)
}

/// When generation `n` was written.
fn written_at(dir: &Path, n: u64) -> SystemTime {
    std::fs::metadata(dir.join(format!("gen-{n}/complete")))
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// What to delete of `written` (oldest first): all but the newest `keep`, and those `used`.
fn deletable(written: &[u64], keep: usize, used: &HashSet<u64>) -> Vec<u64> {
    let old = written.len().saturating_sub(keep);
    written[..old]
        .iter()
        .filter(|n| !used.contains(n))
        .copied()
        .collect()
}

/// What a generation's writer adds, a line each: `signed <path>` (from upstream) or
/// `unsigned <path>` (from the players' cache).
fn todo(paths: &[(String, bool)]) -> String {
    paths
        .iter()
        .map(|(p, signed)| format!("{} {p}\n", if *signed { "signed" } else { "unsigned" }))
        .collect()
}

/// Every 30 s: the current generation, the next one written when it's due, and the old ones
/// deleted.
pub async fn keep(control: &'static Control, kube: &'static Kube, args: &'static NixStoreArgs) {
    let Some(dir) = args.dir.as_deref() else {
        return;
    };
    let mut writing: Option<u64> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    loop {
        tick.tick().await;
        if let Err(e) = round(control, kube, args, dir, &mut writing).await {
            error!("keeping the shared Nix store: {e:#}");
        }
    }
}

async fn round(
    control: &'static Control,
    kube: &'static Kube,
    args: &NixStoreArgs,
    dir: &Path,
    writing: &mut Option<u64>,
) -> anyhow::Result<()> {
    let written = generations(dir)?;
    if let Some(&last) = written.last() {
        if CURRENT.swap(last, Ordering::Relaxed) != last {
            control.pool.wake();
            info!(
                generation = last,
                "VMs get the shared Nix store's generation"
            );
        }
    }
    if let Some(n) = *writing {
        match kube.writer_done(n).await? {
            None => return Ok(()),
            Some(Ok(())) if written.contains(&n) => {
                let todo =
                    std::fs::read_to_string(dir.join(format!("gen-{n}.todo"))).unwrap_or_default();
                let paths: Vec<String> = todo
                    .lines()
                    .filter_map(|l| l.split_once(' ').map(|(_, p)| p.to_string()))
                    .collect();
                control.store.promoted(&paths, n as i64).await?;
                info!(
                    generation = n,
                    paths = paths.len(),
                    "the shared Nix store's generation is written"
                );
            }
            Some(result) => {
                let why = result.err().unwrap_or_else(|| "no `complete` in it".into());
                warn!(
                    generation = n,
                    why, "the shared Nix store's generation wasn't written"
                );
                let _ = std::fs::remove_dir_all(dir.join(format!("gen-{n}.tmp")));
            }
        }
        let _ = std::fs::remove_file(dir.join(format!("gen-{n}.todo")));
        kube.end_writer(n).await;
        *writing = None;
    }
    let last = written.last().copied();
    let due = last.is_none_or(|n| {
        written_at(dir, n).elapsed().unwrap_or_default() >= Duration::from_secs(args.nix_every)
    });
    if due {
        let all = args.nix_promote == Promote::All;
        let paths = control.store.unpromoted(all).await?;
        // the first one, empty, so that VMs have one (and tell what they add)
        if !paths.is_empty() || last.is_none() {
            let n = last.unwrap_or(0) + 1;
            std::fs::write(dir.join(format!("gen-{n}.todo")), todo(&paths))
                .with_context(|| format!("writing gen-{n}.todo"))?;
            kube.start_writer(n, last).await?;
            info!(
                generation = n,
                paths = paths.len(),
                "writing the shared Nix store's generation"
            );
            *writing = Some(n);
        }
    }
    let used = kube.generations_used().await?;
    for n in deletable(&written, args.nix_keep, &used) {
        let gen = dir.join(format!("gen-{n}"));
        info!(generation = n, "deleting the shared Nix store's generation");
        tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&gen))
            .await?
            .with_context(|| format!("deleting gen-{n}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generations_are_those_written_oldest_first() {
        let dir = tempfile::tempdir().unwrap();
        for (name, complete) in [
            ("gen-2", true),
            ("gen-10", true),
            ("gen-11", false),
            ("other", true),
        ] {
            std::fs::create_dir(dir.path().join(name)).unwrap();
            if complete {
                std::fs::write(dir.path().join(name).join("complete"), "").unwrap();
            }
        }
        std::fs::write(dir.path().join("gen-11.todo"), "").unwrap();
        assert_eq!(generations(dir.path()).unwrap(), vec![2, 10]);
    }

    #[test]
    fn old_generations_no_vm_has_are_deleted() {
        let used = HashSet::from([2]);
        assert_eq!(deletable(&[1, 2, 3, 4, 5], 2, &used), vec![1, 3]);
        assert!(deletable(&[1, 2], 3, &used).is_empty());
    }

    #[test]
    fn what_a_writer_adds_says_where_from() {
        let paths = [
            ("/nix/store/a-hello".to_string(), true),
            ("/nix/store/b-mine".to_string(), false),
        ];
        assert_eq!(
            todo(&paths),
            "signed /nix/store/a-hello\nunsigned /nix/store/b-mine\n"
        );
    }
}
