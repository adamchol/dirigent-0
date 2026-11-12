//! The Secrets the chart's pods need, made by `control ensure-secrets` (control's first init
//! container) rather than by Helm, so that the chart renders none: a rendering kept in git has no
//! secrets in it, and rendering it again changes nothing. Each is made once, when it's missing,
//! with fresh random values; one that's there only gets the keys it lacks, and no value is ever
//! replaced (another link key or kopia password would leave the links, keys and checkpoints
//! unreadable). They have no owner, and outlive the release.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::api::{Api, Patch, PatchParams, PostParams};
use rand::RngCore;
use tracing::info;

/// ClickHouse's users for chlive, the ingestion and control, which the `migrate` init container
/// makes (chart/dirigent/templates/_migrate.tpl).
const CLICKHOUSE: [&str; 6] = [
    "chlive_ro",
    "chlive_snap",
    "dirigent_sink",
    "dirigent_pause",
    "dirigent_recovery",
    "dirigent_control",
];

/// A Secret as wanted: its name, its type, and a value for each key, used for the keys it lacks.
struct Wanted {
    name: &'static str,
    kind: Option<&'static str>,
    keys: BTreeMap<String, String>,
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    Create(BTreeMap<String, String>),
    /// The keys it lacks.
    Patch(BTreeMap<String, String>),
    Nothing,
}

/// The chart's Secrets (`garage`: the chart's Garage's too), in the service account's namespace;
/// control's signing and link keys also in `keys` (`key.pem`, `link.key`), when given.
pub async fn ensure(garage: bool, keys: Option<&Path>) -> anyhow::Result<()> {
    let client = kube::Client::try_default()
        .await
        .context("connecting to Kubernetes")?;
    let api: Api<Secret> = Api::default_namespaced(client);
    // the ingestion's Secrets have its users' passwords too: they're taken from this one's, as
    // it is once ensured, so the two always agree
    let users = ensure_one(
        &api,
        &Wanted {
            name: "dirigent-clickhouse",
            kind: None,
            keys: CLICKHOUSE
                .iter()
                .map(|u| (u.to_string(), hex(32)))
                .collect(),
        },
    )
    .await?;
    let mut wanted = vec![
        one(
            "dirigent-signing-key",
            "key.pem",
            crate::token::new_key_pem()?,
        ),
        one("dirigent-link-key", "link.key", crate::seal::new_key()),
        postgres("dirigent-control-postgres", "dirigent_control"),
        postgres("dirigent-chlive-postgres", "dirigent_chlive"),
        one("dirigent-kopia-password", "password", hex(32)),
        Wanted {
            name: crate::runner::NIX_SIGNING_SECRET,
            kind: None,
            keys: nix_signing_key(),
        },
    ];
    for (name, user) in [
        ("dirigent-clickhouse-sink", "dirigent_sink"),
        ("dirigent-clickhouse-pause", "dirigent_pause"),
        ("dirigent-clickhouse-recovery", "dirigent_recovery"),
    ] {
        let password = users
            .get(user)
            .context("dirigent-clickhouse lacks a user")?;
        wanted.push(one(
            name,
            "clickhouse.properties",
            properties(user, password),
        ));
    }
    if garage {
        wanted.push(Wanted {
            name: "dirigent-garage",
            kind: None,
            keys: garage_keys(),
        });
    }
    for w in &wanted {
        let data = ensure_one(&api, w).await?;
        // control's own keys, for its container: a pod's Secret volumes are mounted before its
        // init containers run, so control's pod can't mount these
        if let (Some(dir), "dirigent-signing-key" | "dirigent-link-key") = (keys, w.name) {
            for (key, value) in &data {
                write_key(&dir.join(key), value)?;
            }
        }
    }
    Ok(())
}

/// `value` in `path`, which only its owner may read; again when the init container is.
fn write_key(path: &Path, value: &str) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut f| f.write_all(value.as_bytes()))
        .with_context(|| format!("writing {}", path.display()))
}

/// `w`, made or filled in; its data, as it now is.
async fn ensure_one(api: &Api<Secret>, w: &Wanted) -> anyhow::Result<BTreeMap<String, String>> {
    // again when another run made or changed it in between (a 409)
    for _ in 0..3 {
        let found = api
            .get_opt(w.name)
            .await
            .with_context(|| format!("reading the Secret {}", w.name))?;
        let data = found.as_ref().map(data).transpose()?;
        let result = match fill(data.as_ref(), &w.keys) {
            Action::Nothing => {
                info!("{}: already there", w.name);
                return Ok(data.unwrap_or_default());
            }
            Action::Create(keys) => {
                let secret = Secret {
                    metadata: ObjectMeta {
                        name: Some(w.name.to_string()),
                        labels: Some(BTreeMap::from([
                            ("app.kubernetes.io/name".into(), "dirigent".into()),
                            (
                                "app.kubernetes.io/managed-by".into(),
                                "dirigent-control".into(),
                            ),
                        ])),
                        ..Default::default()
                    },
                    type_: w.kind.map(Into::into),
                    string_data: Some(keys.clone()),
                    ..Default::default()
                };
                api.create(&PostParams::default(), &secret)
                    .await
                    .map(|_| ("made", keys))
            }
            Action::Patch(keys) => {
                // at the version read, or not at all
                let rv = found.and_then(|s| s.metadata.resource_version);
                let patch = serde_json::json!({
                    "metadata": { "resourceVersion": rv },
                    "stringData": keys,
                });
                api.patch(w.name, &PatchParams::default(), &Patch::Merge(&patch))
                    .await
                    .map(|_| ("filled in", keys))
            }
        };
        match result {
            Ok((what, keys)) => {
                info!(
                    "{}: {what} ({})",
                    w.name,
                    keys.keys().cloned().collect::<Vec<_>>().join(", ")
                );
                let mut now = data.unwrap_or_default();
                now.extend(keys);
                return Ok(now);
            }
            Err(kube::Error::Api(e)) if e.code == 409 => continue,
            Err(e) => return Err(e).with_context(|| format!("writing the Secret {}", w.name)),
        }
    }
    anyhow::bail!("the Secret {} kept changing", w.name)
}

/// What to do for a Secret that has `existing` (none: it isn't there), wanting `keys`.
fn fill(existing: Option<&BTreeMap<String, String>>, keys: &BTreeMap<String, String>) -> Action {
    let Some(existing) = existing else {
        return Action::Create(keys.clone());
    };
    let missing: BTreeMap<_, _> = keys
        .iter()
        .filter(|(k, _)| !existing.contains_key(*k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    if missing.is_empty() {
        Action::Nothing
    } else {
        Action::Patch(missing)
    }
}

fn data(secret: &Secret) -> anyhow::Result<BTreeMap<String, String>> {
    let name = secret.metadata.name.as_deref().unwrap_or_default();
    secret
        .data
        .iter()
        .flatten()
        .map(|(k, v)| {
            let v = String::from_utf8(v.0.clone())
                .with_context(|| format!("the Secret {name}'s {k} isn't text"))?;
            Ok((k.clone(), v))
        })
        .collect()
}

fn one(name: &'static str, key: &str, value: String) -> Wanted {
    Wanted {
        name,
        kind: None,
        keys: BTreeMap::from([(key.to_string(), value)]),
    }
}

/// A role's, for CloudNativePG's managed roles (their `passwordSecret`).
fn postgres(name: &'static str, role: &str) -> Wanted {
    Wanted {
        name,
        kind: Some("kubernetes.io/basic-auth"),
        keys: BTreeMap::from([
            ("username".to_string(), role.to_string()),
            ("password".to_string(), hex(32)),
        ]),
    }
}

/// The ingestion's `clickhouse.properties`.
fn properties(user: &str, password: &str) -> String {
    format!("username={user}\npassword={password}\n")
}

/// The chart's Garage's key (its id is GK and 24 hex digits, as Garage's are) and RPC secret.
fn garage_keys() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("accessKey".to_string(), format!("GK{}", hex(12))),
        ("secretKey".to_string(), hex(32)),
        ("rpcSecret".to_string(), hex(32)),
    ])
}

/// The cache's signing key, as Nix has them (`nix-store --generate-binary-cache-key`):
/// DIRIGENT_NIX_SIGNING_KEY, `dirigent-1:<base64 of the Ed25519 seed and public key>`, and
/// DIRIGENT_NIX_KEY, `dirigent-1:<base64 of the public key>`, which players' VMs trust.
fn nix_signing_key() -> BTreeMap<String, String> {
    use base64::Engine;
    let mut seed = [0u8; 32];
    rand::rng().fill_bytes(&mut seed);
    let key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let b64 = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
    BTreeMap::from([
        (
            "DIRIGENT_NIX_SIGNING_KEY".to_string(),
            format!("dirigent-1:{}", b64(&key.to_keypair_bytes())),
        ),
        (
            "DIRIGENT_NIX_KEY".to_string(),
            format!("dirigent-1:{}", b64(key.verifying_key().as_bytes())),
        ),
    ])
}

/// `n` random bytes, in hex.
fn hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn the_caches_signing_key_is_nixs_with_its_public_half() {
        let keys = nix_signing_key();
        let mut nix = std::process::Command::new("nix")
            .args(["--extra-experimental-features", "nix-command", "key"])
            .arg("convert-secret-to-public")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        let secret = &keys["DIRIGENT_NIX_SIGNING_KEY"];
        nix.stdin
            .take()
            .unwrap()
            .write_all(secret.as_bytes())
            .unwrap();
        let out = nix.wait_with_output().unwrap();
        assert!(out.status.success(), "{out:?}");
        assert_eq!(
            String::from_utf8(out.stdout).unwrap().trim(),
            keys["DIRIGENT_NIX_KEY"]
        );
        assert_ne!(nix_signing_key(), keys);
    }

    #[test]
    fn fill_makes_what_is_missing_and_keeps_what_is_there() {
        let wanted = map(&[("a", "new a"), ("b", "new b")]);
        assert_eq!(fill(None, &wanted), Action::Create(wanted.clone()));
        assert_eq!(
            fill(Some(&map(&[("a", "old a"), ("b", "old b")])), &wanted),
            Action::Nothing
        );
        // only the missing key, and the one there unchanged
        assert_eq!(
            fill(Some(&map(&[("a", "old a"), ("c", "other")])), &wanted),
            Action::Patch(map(&[("b", "new b")]))
        );
    }

    #[test]
    fn values_are_in_their_formats() {
        let dir = tempfile::tempdir().unwrap();
        let pem = dir.path().join("key.pem");
        std::fs::write(&pem, crate::token::new_key_pem().unwrap()).unwrap();
        crate::token::Signer::load(&pem, "", Duration::ZERO).unwrap();
        let link = dir.path().join("link.key");
        std::fs::write(&link, crate::seal::new_key()).unwrap();
        crate::seal::Seal::load(&link).unwrap();

        let hexes = |s: &str, n| s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit());
        let garage = garage_keys();
        let id = &garage["accessKey"];
        assert!(id.starts_with("GK") && hexes(&id[2..], 24), "{id}");
        assert!(hexes(&garage["secretKey"], 64));
        assert!(hexes(&garage["rpcSecret"], 64));
        assert_ne!(hex(32), hex(32));
    }

    #[test]
    fn properties_are_the_users() {
        assert_eq!(
            properties("dirigent_sink", "pw"),
            "username=dirigent_sink\npassword=pw\n"
        );
    }
}
