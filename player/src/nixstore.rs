//! What this VM added to the shared Nix store (the chart's `nix.shared`; nix/vm/base.nix): the
//! store paths in its own layer (the overlay's upper), but the image's, told to control, which
//! puts them in the store's next generation, under every later VM's own layer; so those have
//! them without fetching them. Each is told with whether the upstream cache signed it: with
//! `all` (DIRIGENT_NIX_PROMOTE), those it didn't are put in the players' cache too
//! (DIRIGENT_NIX_CACHE), where the generation takes them from. Best effort: what isn't told
//! isn't shared.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context;
use serde_json::{json, Value};
use tracing::{info, warn};

use crate::process::{command, run};

/// The VM's own layer of the store, and the image's paths in it (written at the first boot).
const UPPER: &str = "/nix/.upper/store";
const IMAGE_PATHS: &str = "/nix/.upper/image-paths";
/// The key upstream signs with, whose paths any generation takes.
const UPSTREAM_KEY: &str = "cache.nixos.org-1:";
const NIX_TIMEOUT: Duration = Duration::from_secs(600);

pub struct Reporter {
    control_http: String,
    token: Box<dyn Fn() -> String + Send + Sync>,
    /// The players' cache, which paths upstream didn't sign go to, with `all`.
    cache: Option<String>,
    reported: HashSet<String>,
    http: reqwest::Client,
}

impl Reporter {
    /// For a VM on the shared store (DIRIGENT_NIX_GENERATION), telling control at
    /// `control_http`, with the player's `token`.
    pub fn new(
        control_http: &str,
        token: impl Fn() -> String + Send + Sync + 'static,
    ) -> Option<Reporter> {
        std::env::var_os("DIRIGENT_NIX_GENERATION")?;
        let all = std::env::var("DIRIGENT_NIX_PROMOTE").is_ok_and(|p| p == "all");
        Some(Reporter {
            control_http: control_http.trim_end_matches('/').to_string(),
            token: Box::new(token),
            cache: std::env::var("DIRIGENT_NIX_CACHE").ok().filter(|_| all),
            reported: HashSet::new(),
            http: reqwest::Client::new(),
        })
    }

    /// Tell control what was added since the last time.
    pub async fn report(&mut self) {
        if let Err(e) = self.report_once().await {
            warn!("telling control of the store paths this VM added: {e:#}");
        }
    }

    async fn report_once(&mut self) -> anyhow::Result<()> {
        let image: HashSet<String> = std::fs::read_to_string(IMAGE_PATHS)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect();
        let added: Vec<String> = added(UPPER)?
            .into_iter()
            .filter(|p| !image.contains(p) && !self.reported.contains(p))
            .collect();
        if added.is_empty() {
            return Ok(());
        }
        let mut cmd = command("nix");
        cmd.args(["path-info", "--json"]).args(&added);
        let infos = run(cmd, NIX_TIMEOUT).await.unwrap_or_default();
        let paths = signed(&infos);
        if let Some(cache) = &self.cache {
            let unsigned: Vec<&str> = paths
                .iter()
                .filter(|(_, signed)| !signed)
                .map(|(p, _)| p.as_str())
                .collect();
            if !unsigned.is_empty() {
                let mut cmd = command("nix");
                cmd.args(["copy", "--to", cache]).args(&unsigned);
                run(cmd, NIX_TIMEOUT)
                    .await
                    .map_err(|e| anyhow::anyhow!("putting them in the cache: {e}"))?;
            }
        }
        let body = json!({
            "paths": paths
                .iter()
                .map(|(path, signed)| json!({ "path": path, "signed": signed }))
                .collect::<Vec<_>>(),
        });
        let res = self
            .http
            .post(format!("{}/nix/paths", self.control_http))
            .bearer_auth((self.token)())
            .json(&body)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .context("reaching control")?;
        anyhow::ensure!(
            res.status().is_success(),
            "control answered {}",
            res.status()
        );
        info!(
            paths = paths.len(),
            "told control of the store paths this VM added"
        );
        // the invalid ones (being written) are looked at again next time
        self.reported.extend(paths.into_iter().map(|(p, _)| p));
        Ok(())
    }
}

/// The store paths in `upper`, the store's own layer.
fn added(upper: &str) -> anyhow::Result<Vec<String>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(upper).with_context(|| format!("reading {upper}"))? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if !name.starts_with('.') {
            paths.push(format!("/nix/store/{name}"));
        }
    }
    paths.sort();
    Ok(paths)
}

/// The valid paths in `nix path-info --json`'s answer, and whether upstream signed each: an
/// object by path (null for an invalid one), or a list, as Nix's versions have it.
fn signed(infos: &str) -> Vec<(String, bool)> {
    let upstream = |info: &Value| {
        info["signatures"].as_array().is_some_and(|s| {
            s.iter()
                .any(|s| s.as_str().is_some_and(|s| s.starts_with(UPSTREAM_KEY)))
        })
    };
    match serde_json::from_str::<Value>(infos).unwrap_or(Value::Null) {
        Value::Object(by_path) => by_path
            .into_iter()
            .filter(|(_, info)| info.is_object())
            .map(|(path, info)| (path, upstream(&info)))
            .collect(),
        Value::Array(list) => list
            .iter()
            .filter(|info| info["valid"].as_bool() != Some(false))
            .filter_map(|info| Some((info["path"].as_str()?.to_string(), upstream(info))))
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_valid_paths_and_whether_upstream_signed_them() {
        let by_path = r#"{
            "/nix/store/a-hello": { "signatures": ["cache.nixos.org-1:xyz"] },
            "/nix/store/b-mine": { "signatures": ["dirigent-1:abc"] },
            "/nix/store/c-partial": null
        }"#;
        let mut got = signed(by_path);
        got.sort();
        assert_eq!(
            got,
            vec![
                ("/nix/store/a-hello".to_string(), true),
                ("/nix/store/b-mine".to_string(), false)
            ]
        );
        let list = r#"[
            { "path": "/nix/store/a-hello", "valid": true, "signatures": ["cache.nixos.org-1:xyz"] },
            { "path": "/nix/store/c-partial", "valid": false }
        ]"#;
        assert_eq!(signed(list), vec![("/nix/store/a-hello".to_string(), true)]);
        assert!(signed("not json").is_empty());
    }

    #[test]
    fn the_paths_added_are_the_upper_layers_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("abc-hello")).unwrap();
        std::fs::write(dir.path().join("def-file"), "x").unwrap();
        std::fs::create_dir(dir.path().join(".links")).unwrap();
        assert_eq!(
            added(dir.path().to_str().unwrap()).unwrap(),
            vec!["/nix/store/abc-hello", "/nix/store/def-file"]
        );
    }
}
