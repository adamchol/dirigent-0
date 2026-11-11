//! Where control starts the players it runs: here, as local processes, or in KubeVirt VMs. Only
//! starting is the runner's: a player's liveness is its lease, and it stops on control's word
//! (`Detach`), or hands the session over when its machine goes, so the runner neither watches
//! nor kills it (the KubeVirt runner only clears away the VMs that are done).

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use kube::api::{Api, ApiResource, DeleteParams, DynamicObject, ListParams, PostParams};
use tokio::process::Command;
use tracing::{error, info, warn};
use uuid::Uuid;

use dirigent_protocol::control::player_subject;

use crate::leases::Store;

#[async_trait::async_trait]
pub trait Runner: Send + Sync {
    /// Which runner it is, in the leases of the players it started: `local:<root>`,
    /// `kube:<namespace>/<name>`. A control restarts only those of its own runner.
    fn id(&self) -> &str;

    /// Start `player` (an id) on `session`, where the lease for its `epoch` waits for it.
    async fn start(&self, session: Uuid, player: &str, epoch: i64) -> anyhow::Result<()>;

    /// The session `session` is deleted: remove what its players left here (none holds it).
    async fn forget(&self, session: Uuid) -> anyhow::Result<()>;
}

/// The environment a player gets: what it needs, and none of control's own (its Postgres).
const PASSED_ON: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "RUST_LOG",
    "CHLIVE_URL",
    "DIRIGENT_KOPIA_TOKEN",
    "ANTHROPIC_BASE_URL",
];

/// Players as processes on this machine, each session's in `<root>/<session>`: its workspace,
/// the player's state, and its output (`player.log`). They outlive a control that stops (and
/// resume their leases when it's back), as VMs would.
pub struct Local {
    pub id: String,
    pub root: PathBuf,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Control's gRPC service for players.
    pub control_url: String,
    /// Control's HTTP API, which dirigents direct sessions through.
    pub control_http: String,
}

#[async_trait::async_trait]
impl Runner for Local {
    fn id(&self) -> &str {
        &self.id
    }

    async fn start(&self, session: Uuid, player: &str, epoch: i64) -> anyhow::Result<()> {
        let dir = self.root.join(session.to_string());
        std::fs::create_dir_all(&dir).with_context(|| format!("making {}", dir.display()))?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("player.log"))
            .with_context(|| format!("opening {}/player.log", dir.display()))?;
        let mut cmd = Command::new(&self.program);
        cmd.arg("--session")
            .arg(session.to_string())
            .args(["--player-id", player])
            .args(["--epoch", &epoch.to_string()])
            .args(["--control-url", &self.control_url])
            .args(["--control-http", &self.control_http])
            .arg("--workdir")
            .arg(dir.join("workspace"))
            .arg("--state-dir")
            .arg(dir.join("state"))
            .args(&self.args)
            .env_clear()
            .envs(
                PASSED_ON
                    .iter()
                    .filter_map(|k| Some((k, std::env::var_os(k)?))),
            )
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("running {}", self.program.display()))?;
        let pid = child.id();
        info!(session = %session, player, epoch, pid, "player started");
        let player = player.to_string();
        tokio::spawn(async move {
            match child.wait().await {
                Ok(status) if status.success() => {
                    info!(session = %session, player, "player exited")
                }
                Ok(status) => warn!(session = %session, player, %status, "player failed"),
                Err(e) => warn!(session = %session, player, "waiting for the player: {e}"),
            }
        });
        Ok(())
    }

    async fn forget(&self, session: Uuid) -> anyhow::Result<()> {
        let dir = self.root.join(session.to_string());
        match std::fs::remove_dir_all(&dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                Err(e).with_context(|| format!("removing {}", dir.display()))
            }
            _ => Ok(()),
        }
    }
}

/// The volume of a player's VM template whose NoCloud user data is the player's environment
/// (an env file); the template's lines (control's and chlive's addresses, the player's options)
/// come first, then the player's own.
pub const CONFIG_VOLUME: &str = "dirigent";
const LABEL_SESSION: &str = "dirigent.dev/session";
const LABEL_PLAYER: &str = "dirigent.dev/player";
const LABEL_EPOCH: &str = "dirigent.dev/epoch";
const LABEL_RUNNER: &str = "dirigent.dev/runner";

/// Players in KubeVirt VMs: a `VirtualMachineInstance` each, `player-<session>-<epoch>`, from a
/// template (Helm's): its image, size, disks, grace period and the Secrets it gets. The VM
/// powers off when its player exits; [`Kube::clear`] deletes it then.
pub struct Kube {
    id: String,
    /// Its VMs' `dirigent.dev/runner` label.
    name: String,
    vmis: Api<DynamicObject>,
    template: serde_json::Value,
}

impl Kube {
    /// In `namespace` (the service account's when absent), from the VMI `template` (JSON).
    pub async fn new(
        name: &str,
        namespace: Option<&str>,
        template: &std::path::Path,
    ) -> anyhow::Result<Kube> {
        let text = std::fs::read_to_string(template)
            .with_context(|| format!("reading the VM template {}", template.display()))?;
        let template: serde_json::Value = serde_json::from_str(&text).with_context(|| {
            format!("{}: not a JSON VirtualMachineInstance", template.display())
        })?;
        // checked once, here: every VM is made from it
        vmi(&template, name, Uuid::nil(), "", 0)?;
        let client = kube::Client::try_default()
            .await
            .context("connecting to Kubernetes")?;
        let namespace = namespace.unwrap_or(client.default_namespace()).to_string();
        let vmis = Api::namespaced_with(client, &namespace, &vmi_resource());
        Ok(Kube {
            id: format!("kube:{namespace}/{name}"),
            name: name.to_string(),
            vmis,
            template,
        })
    }

    /// Every `every`, delete this runner's VMs that are done (powered off, or failed), and those
    /// past their `startup` whose player holds no lease (it never came, or lost it).
    pub async fn clear(&'static self, store: &'static Store, startup: Duration, every: Duration) {
        let mut tick = tokio::time::interval(every);
        loop {
            tick.tick().await;
            if let Err(e) = self.clear_once(store, startup).await {
                error!("clearing away the players' VMs: {e:#}");
            }
        }
    }

    async fn clear_once(&self, store: &Store, startup: Duration) -> anyhow::Result<()> {
        let held: HashSet<String> = store.players(&self.id).await?.into_iter().collect();
        let mine = ListParams::default().labels(&format!("{LABEL_RUNNER}={}", self.name));
        for vmi in self.vmis.list(&mine).await? {
            let name = vmi.metadata.name.clone().unwrap_or_default();
            let phase = vmi.data["status"]["phase"].as_str().unwrap_or("");
            let player = vmi
                .metadata
                .labels
                .as_ref()
                .and_then(|l| l.get(LABEL_PLAYER))
                .map(|p| player_subject(p));
            let old = vmi
                .metadata
                .creation_timestamp
                .as_ref()
                .is_some_and(|t| age(t) > startup);
            let why = match phase {
                "Succeeded" | "Failed" => phase,
                _ if old && !player.is_some_and(|p| held.contains(&p)) => "holds no lease",
                _ => continue,
            };
            info!(vmi = name, why, "deleting a player's VM");
            match self.vmis.delete(&name, &DeleteParams::default()).await {
                Ok(_) => {}
                Err(kube::Error::Api(e)) if e.code == 404 => {}
                Err(e) => warn!(vmi = name, "deleting: {e}"),
            }
        }
        Ok(())
    }
}

/// How long ago `t` was.
fn age(t: &k8s_openapi::apimachinery::pkg::apis::meta::v1::Time) -> Duration {
    let then = std::time::SystemTime::from(t.0);
    std::time::SystemTime::now()
        .duration_since(then)
        .unwrap_or_default()
}

#[async_trait::async_trait]
impl Runner for Kube {
    fn id(&self) -> &str {
        &self.id
    }

    async fn start(&self, session: Uuid, player: &str, epoch: i64) -> anyhow::Result<()> {
        let vmi = vmi(&self.template, &self.name, session, player, epoch)?;
        let name = vmi.metadata.name.clone().unwrap_or_default();
        match self.vmis.create(&PostParams::default(), &vmi).await {
            Ok(_) => {}
            // made by an earlier try for this same epoch
            Err(kube::Error::Api(e)) if e.code == 409 => {}
            Err(e) => return Err(e).with_context(|| format!("making the VM {name}")),
        }
        info!(session = %session, player, epoch, vmi = name, "player started");
        Ok(())
    }

    async fn forget(&self, session: Uuid) -> anyhow::Result<()> {
        let its = ListParams::default().labels(&format!(
            "{LABEL_RUNNER}={},{LABEL_SESSION}={session}",
            self.name
        ));
        for vmi in self.vmis.list(&its).await? {
            let name = vmi.metadata.name.unwrap_or_default();
            match self.vmis.delete(&name, &DeleteParams::default()).await {
                Ok(_) => info!(session = %session, vmi = name, "deleted a deleted session's VM"),
                Err(kube::Error::Api(e)) if e.code == 404 => {}
                Err(e) => return Err(e).with_context(|| format!("deleting the VM {name}")),
            }
        }
        Ok(())
    }
}

fn vmi_resource() -> ApiResource {
    ApiResource {
        group: "kubevirt.io".into(),
        version: "v1".into(),
        api_version: "kubevirt.io/v1".into(),
        kind: "VirtualMachineInstance".into(),
        plural: "virtualmachineinstances".into(),
    }
}

/// The VM for `player` on `session`, from `template`: named and labelled for them, its config
/// volume's user data with their environment after the template's.
fn vmi(
    template: &serde_json::Value,
    runner: &str,
    session: Uuid,
    player: &str,
    epoch: i64,
) -> anyhow::Result<DynamicObject> {
    let mut vmi = template.clone();
    anyhow::ensure!(
        vmi["kind"] == "VirtualMachineInstance",
        "the VM template is a {}, not a VirtualMachineInstance",
        vmi["kind"]
    );
    let volumes = vmi["spec"]["volumes"]
        .as_array_mut()
        .context("the VM template has no spec.volumes")?;
    let config = volumes
        .iter_mut()
        .find(|v| v["name"] == CONFIG_VOLUME)
        .and_then(|v| v.get_mut("cloudInitNoCloud"))
        .with_context(|| {
            format!("the VM template has no volume {CONFIG_VOLUME:?} with cloudInitNoCloud")
        })?;
    let mut env = config["userData"].as_str().unwrap_or_default().to_string();
    if !env.is_empty() && !env.ends_with('\n') {
        env.push('\n');
    }
    env.push_str(&format!(
        "DIRIGENT_SESSION={session}\nDIRIGENT_PLAYER_ID={player}\nDIRIGENT_EPOCH={epoch}\n"
    ));
    config["userData"] = env.into();
    let meta = &mut vmi["metadata"];
    if !meta.is_object() {
        *meta = serde_json::json!({});
    }
    meta["name"] = format!("player-{session}-{epoch}").into();
    let labels = &mut meta["labels"];
    if !labels.is_object() {
        *labels = serde_json::json!({});
    }
    labels[LABEL_SESSION] = session.to_string().into();
    labels[LABEL_PLAYER] = player.into();
    labels[LABEL_EPOCH] = epoch.to_string().into();
    labels[LABEL_RUNNER] = runner.into();
    Ok(serde_json::from_value(vmi)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template() -> serde_json::Value {
        serde_json::json!({
            "apiVersion": "kubevirt.io/v1",
            "kind": "VirtualMachineInstance",
            "metadata": { "labels": { "app.kubernetes.io/name": "dirigent-player" } },
            "spec": {
                "terminationGracePeriodSeconds": 3600,
                "volumes": [
                    { "name": "disk", "containerDisk": { "image": "player-vm" } },
                    { "name": CONFIG_VOLUME, "cloudInitNoCloud": {
                        "userData": "DIRIGENT_CONTROL_URL=http://control:8091"
                    } },
                ],
            },
        })
    }

    #[test]
    fn a_players_vm_is_the_template_named_and_labelled_with_its_environment() {
        let s = Uuid::new_v4();
        let vmi = vmi(&template(), "dirigent", s, "p1", 3).unwrap();
        assert_eq!(
            vmi.metadata.name.as_deref(),
            Some(format!("player-{s}-3").as_str())
        );
        let labels = vmi.metadata.labels.unwrap();
        assert_eq!(labels[LABEL_SESSION], s.to_string());
        assert_eq!(labels[LABEL_PLAYER], "p1");
        assert_eq!(labels[LABEL_EPOCH], "3");
        assert_eq!(labels[LABEL_RUNNER], "dirigent");
        assert_eq!(labels["app.kubernetes.io/name"], "dirigent-player");
        assert_eq!(vmi.data["spec"]["terminationGracePeriodSeconds"], 3600);
        assert_eq!(
            vmi.data["spec"]["volumes"][1]["cloudInitNoCloud"]["userData"],
            format!(
                "DIRIGENT_CONTROL_URL=http://control:8091\nDIRIGENT_SESSION={s}\n\
                 DIRIGENT_PLAYER_ID=p1\nDIRIGENT_EPOCH=3\n"
            )
        );
    }

    #[test]
    fn a_template_without_the_config_volume_is_refused() {
        let mut t = template();
        t["spec"]["volumes"].as_array_mut().unwrap().pop();
        let e = vmi(&t, "d", Uuid::nil(), "", 0).unwrap_err();
        assert!(e.to_string().contains("no volume"), "{e}");
        let mut t = template();
        t["kind"] = "VirtualMachine".into();
        assert!(vmi(&t, "d", Uuid::nil(), "", 0).is_err());
    }
}
