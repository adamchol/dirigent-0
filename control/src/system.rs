//! The cluster's own players' system: what its VMs run when the chart has a configuration of its
//! own (`player.nixos.files`, mounted at `--nixos-config`), built ahead of time, by the runner
//! (a build VM: nix/vm/build.nix), once per configuration and players' VM image
//! ([`VM`]). A redeploy that changed neither builds nothing; a new configuration or image is built
//! at control's start.
//! Until it's built, or when it can't be, sessions run the stock image. A session whose VM didn't
//! come up with it is started again on the stock image: if that one does, the system is broken,
//! and stays so until the configuration or the image changes (if it doesn't either, something
//! else is wrong). Only a runner that runs it counts ([`crate::runner::Runner::runs_systems`]).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use rand::RngCore;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::Control;

/// The players' VM image a system is built for: its NixOS system's (the flake's, nix/vm); a
/// cargo build's, its version. Not the player's, which comes on a disk of its own.
pub const VM: &str = match option_env!("DIRIGENT_VM") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

pub struct System {
    /// Where the configuration's files are.
    pub dir: PathBuf,
    /// sha256 of the files (their names and contents) and [`VM`].
    pub hash: String,
    /// How long a build may take.
    timeout: Duration,
    /// The built system's store path, once there's one.
    ready: Mutex<Option<String>>,
    /// The sessions started with it whose player hasn't come yet, and the epoch it's to take.
    waiting: Mutex<HashMap<Uuid, i64>>,
    /// Those whose player, started with it, never came: started again on the stock image (at the
    /// epoch, once it is), to tell whether the system is why.
    suspects: Mutex<HashMap<Uuid, Option<i64>>>,
}

/// How it goes, as `GET /system` tells.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct Status {
    /// Whether the cluster has a configuration of its own; without, sessions run the stock image.
    pub configured: bool,
    /// `building`, `ready`, `failed` or `broken`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl System {
    /// The configuration in `dir`: its files (none: the stock image's, so no system of its own).
    pub fn load(dir: &Path, timeout: Duration) -> anyhow::Result<Option<System>> {
        let files = files(dir)?;
        if files.is_empty() {
            return Ok(None);
        }
        anyhow::ensure!(
            files.contains_key("configuration.nix"),
            "{} has no configuration.nix",
            dir.display()
        );
        let dir = dir
            .canonicalize()
            .with_context(|| format!("{}", dir.display()))?;
        Ok(Some(System {
            dir,
            hash: hash(&files, VM),
            timeout,
            ready: Mutex::new(None),
            waiting: Mutex::new(HashMap::new()),
            suspects: Mutex::new(HashMap::new()),
        }))
    }

    /// The system to start a session's player in (`session` at `epoch`), once there's one, noted as
    /// started with it until its player comes ([`System::came`]); but the stock image for a
    /// suspect.
    pub fn start(&self, session: Uuid, epoch: i64) -> Option<String> {
        if let Some(stock) = self.suspects.lock().unwrap().get_mut(&session) {
            *stock = Some(epoch);
            return None;
        }
        let ready = self.ready.lock().unwrap().clone();
        if ready.is_some() {
            self.waiting.lock().unwrap().insert(session, epoch);
        }
        ready
    }

    /// The built system, if there's one (what a warm VM is made in).
    pub fn ready(&self) -> Option<String> {
        self.ready.lock().unwrap().clone()
    }

    /// Whether `session` is a suspect: started again on the stock image, not in the system.
    pub fn suspect(&self, session: Uuid) -> bool {
        self.suspects.lock().unwrap().contains_key(&session)
    }

    /// The player that was to take `session` at `epoch` came: its VM came up. Whether it's a
    /// suspect's, on the stock image, so that the system is why the one before didn't.
    pub fn came(&self, session: Uuid, epoch: i64) -> bool {
        let mut waiting = self.waiting.lock().unwrap();
        if waiting.get(&session) == Some(&epoch) {
            waiting.remove(&session);
        }
        let mut suspects = self.suspects.lock().unwrap();
        if suspects.get(&session) == Some(&Some(epoch)) {
            suspects.remove(&session);
            return true;
        }
        false
    }

    /// `session`'s player never came: started with the system, it's a suspect now; already one
    /// (on the stock image), it isn't any more, since that didn't come either. Whether it's one.
    fn never_came(&self, session: Uuid) -> bool {
        let mut suspects = self.suspects.lock().unwrap();
        if suspects.remove(&session).is_some() {
            return false;
        }
        if self.waiting.lock().unwrap().remove(&session).is_some() {
            suspects.insert(session, None);
            return true;
        }
        false
    }
}

/// The regular files in `dir`, by name.
fn files(dir: &Path) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // a ConfigMap's mount has its files as links into a hidden `..data` directory
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        if path.is_file() {
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            files.insert(name, bytes);
        }
    }
    Ok(files)
}

/// What a configuration's system is built from: its files and dirigent's version.
fn hash(files: &BTreeMap<String, Vec<u8>>, version: &str) -> String {
    let mut sha = Sha256::new();
    for (name, bytes) in files {
        // lengths first, so no two sets of files hash alike
        sha.update((name.len() as u64).to_be_bytes());
        sha.update(name.as_bytes());
        sha.update((bytes.len() as u64).to_be_bytes());
        sha.update(bytes);
    }
    sha.update(version.as_bytes());
    hex(&sha.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// At control's start: the system built for this configuration and version, or a build of it.
pub async fn begin(control: &'static Control) {
    let Some(system) = &control.system else {
        return;
    };
    let found = match control.store.player_system(&system.hash).await {
        Ok(found) => found,
        Err(e) => {
            error!("reading the players' system: {e:#}");
            return;
        }
    };
    match found {
        Some(s) if s.state == "ready" => {
            let path = s.store_path.unwrap_or_default();
            info!(hash = system.hash, path, "the players' system is built");
            *system.ready.lock().unwrap() = Some(path);
        }
        Some(s) if s.state == "broken" => warn!(
            hash = system.hash,
            why = s.error.as_deref().unwrap_or_default(),
            "the players' system is broken: sessions run the stock image until its configuration \
             or dirigent's version changes"
        ),
        // never built, failed (a restart is how to try again), or left building by an earlier
        // control
        _ => build(control, system).await,
    }
}

async fn build(control: &'static Control, system: &'static System) {
    let Some(runner) = control.runner else {
        return;
    };
    let id = Uuid::new_v4();
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let token = hex(&bytes);
    if let Err(e) = control
        .store
        .build_system(&system.hash, id, &digest(&token))
        .await
    {
        error!("noting the players' system's build: {e:#}");
        return;
    }
    info!(hash = system.hash, build = %id, "building the players' system");
    runner
        .note(false, "Building", "building the players' system")
        .await;
    if let Err(e) = runner.build(id, &token, &system.dir).await {
        failed(control, id, &format!("{e:#}")).await;
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(system.timeout).await;
        let why = format!("not built in {}s", system.timeout.as_secs());
        failed(control, id, &why).await;
    });
}

/// The build `id` failed without telling, unless it's done.
async fn failed(control: &'static Control, id: Uuid, why: &str) {
    match control.store.system_failed(id, why).await {
        Ok(true) => {
            error!(build = %id, why, "the players' system wasn't built: sessions run the stock image");
            if let Some(runner) = control.runner {
                runner.note(true, "BuildFailed", why).await;
                runner.end_build(id).await;
            }
        }
        Ok(false) => {}
        Err(e) => error!(build = %id, "noting the players' system's failed build: {e:#}"),
    }
}

/// What a build VM tells of its build.
#[derive(Debug, serde::Deserialize)]
pub struct Report {
    pub ok: bool,
    #[serde(default)]
    pub store_path: Option<String>,
    #[serde(default)]
    pub log: Option<String>,
}

/// The build `id` tells how it went, with `token`. Whether it was the build under way and
/// that its token (each tells once).
pub async fn built(
    control: &'static Control,
    id: Uuid,
    token: &str,
    report: &Report,
) -> anyhow::Result<bool> {
    let Some(system) = &control.system else {
        return Ok(false);
    };
    let result = match (report.ok, &report.store_path) {
        (true, Some(path)) if path.starts_with("/nix/store/") => Ok(path.as_str()),
        (true, _) => Err("the build told no store path"),
        (false, _) => Err(report.log.as_deref().unwrap_or("the build failed")),
    };
    if !control
        .store
        .system_built(id, &digest(token), result)
        .await?
    {
        return Ok(false);
    }
    let runner = control.runner;
    match result {
        Ok(path) => {
            info!(build = %id, path, "the players' system is built");
            *system.ready.lock().unwrap() = Some(path.to_string());
            control.pool.wake();
            if let Some(runner) = runner {
                runner.note(false, "Built", path).await;
            }
        }
        Err(log) => {
            error!(build = %id, "the players' system wasn't built: sessions run the stock image\n{log}");
            if let Some(runner) = runner {
                runner.note(true, "BuildFailed", log).await;
            }
        }
    }
    if let Some(runner) = runner {
        runner.end_build(id).await;
    }
    Ok(true)
}

/// `session`'s player, started by control, was lost before it came: started with the system,
/// the session is started again on the stock image, to tell whether the system is why.
pub fn lost(control: &'static Control, session: Uuid) {
    if let Some(system) = &control.system {
        if system.never_came(session) {
            warn!(
                session = %session,
                "its VM didn't come up with the players' system: starting it on the stock image"
            );
        }
    }
}

/// A session whose VM didn't come up with the system came up on the stock image: the system is
/// broken.
pub async fn broken(control: &'static Control, session: Uuid) {
    let Some(system) = &control.system else {
        return;
    };
    let why = format!("a session's VM ({session}) didn't come up with it, and did without");
    *system.ready.lock().unwrap() = None;
    system.waiting.lock().unwrap().clear();
    system.suspects.lock().unwrap().clear();
    control.pool.wake();
    error!(
        hash = system.hash,
        why, "the players' system is broken: sessions run the stock image"
    );
    if let Err(e) = control.store.system_broken(&system.hash, &why).await {
        error!("noting the players' system broken: {e:#}");
    }
    if let Some(runner) = control.runner {
        runner.note(true, "Broken", &why).await;
    }
}

/// How it goes.
pub async fn status(control: &'static Control) -> anyhow::Result<Status> {
    let Some(system) = &control.system else {
        return Ok(Status {
            configured: false,
            state: None,
            store_path: None,
            error: None,
        });
    };
    let found = control.store.player_system(&system.hash).await?;
    Ok(Status {
        configured: true,
        state: found.as_ref().map(|s| s.state.clone()),
        store_path: found.as_ref().and_then(|s| s.store_path.clone()),
        error: found.and_then(|s| s.error),
    })
}

fn digest(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_systems_hash_is_its_files_and_the_version() {
        let files = |list: &[(&str, &str)]| -> BTreeMap<String, Vec<u8>> {
            list.iter()
                .map(|(n, c)| (n.to_string(), c.as_bytes().to_vec()))
                .collect()
        };
        let base = files(&[("configuration.nix", "{ }"), ("node.nix", "{ }")]);
        assert_eq!(hash(&base, "1"), hash(&base.clone(), "1"));
        for other in [
            files(&[("configuration.nix", "{ }"), ("node.nix", "{ x = 1; }")]),
            files(&[("configuration.nix", "{ }"), ("nodes.nix", "{ }")]),
            files(&[("configuration.nix", "{ }")]),
            // the same bytes, split otherwise between name and content
            files(&[("configuration.nix", "{ }"), ("node.ni", "x{ }")]),
        ] {
            assert_ne!(hash(&base, "1"), hash(&other, "1"), "{other:?}");
        }
        assert_ne!(hash(&base, "1"), hash(&base, "2"));
    }

    #[test]
    fn a_configuration_is_its_directorys_files_with_configuration_nix() {
        let dir = tempfile::tempdir().unwrap();
        let t = Duration::from_secs(1);
        assert!(System::load(dir.path(), t).unwrap().is_none());
        std::fs::write(dir.path().join("node.nix"), "{ }").unwrap();
        assert!(System::load(dir.path(), t).is_err());
        std::fs::write(dir.path().join("configuration.nix"), "{ }").unwrap();
        // a ConfigMap's own bookkeeping isn't a file of it
        std::fs::create_dir(dir.path().join("..data")).unwrap();
        std::fs::write(dir.path().join(".hidden"), "x").unwrap();
        let system = System::load(dir.path(), t).unwrap().unwrap();
        let both = BTreeMap::from([
            ("configuration.nix".to_string(), b"{ }".to_vec()),
            ("node.nix".to_string(), b"{ }".to_vec()),
        ]);
        assert_eq!(system.hash, hash(&both, VM));
    }

    #[test]
    fn a_vm_that_doesnt_come_up_with_the_system_but_does_without_says_its_broken() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("configuration.nix"), "{ }").unwrap();
        let system = System::load(dir.path(), Duration::from_secs(1))
            .unwrap()
            .unwrap();
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let x = Some("/nix/store/x".to_string());
        // none built: nothing waits for it
        assert_eq!(system.start(a, 1), None);
        assert!(!system.never_came(a));
        *system.ready.lock().unwrap() = x.clone();

        // came up with it
        assert_eq!(system.start(a, 2), x);
        assert!(!system.came(a, 2));
        assert!(!system.never_came(a));

        // didn't (an older epoch's player coming isn't this one's), then did on the stock image
        assert_eq!(system.start(b, 1), x);
        assert!(!system.came(b, 0));
        assert!(system.never_came(b));
        assert_eq!(system.start(b, 2), None);
        assert!(system.came(b, 2), "the system is why");

        // didn't either way: not the system's fault, and it's tried again
        assert_eq!(system.start(c, 1), x);
        assert!(system.never_came(c));
        assert_eq!(system.start(c, 2), None);
        assert!(!system.never_came(c));
        assert_eq!(system.start(c, 3), x);
    }
}
