//! Where control starts the players it runs: here, as local processes, or in KubeVirt VMs. Only
//! starting is the runner's: a player's liveness is its lease, and it stops on control's word
//! (`Detach`), or hands the session over when its machine goes, so the runner neither watches
//! nor kills it (the KubeVirt runner only clears away the VMs that are done). It also builds the
//! cluster's own players' system, where it has one ([`crate::system`]).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::{Event, ObjectReference, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
use kube::api::{
    Api, ApiResource, DeleteParams, DynamicObject, ListParams, Patch, PatchParams, PostParams,
};
use tokio::process::Command;
use tracing::{error, info, warn};
use uuid::Uuid;

use dirigent_protocol::control::{player_subject, Resources};

use crate::leases::Store;

#[async_trait::async_trait]
pub trait Runner: Send + Sync {
    /// Which runner it is, in the leases of the players it started: `local:<root>`,
    /// `kube:<namespace>/<name>`. A control restarts only those of its own runner.
    fn id(&self) -> &str;

    /// Start `player` (an id) on `session`, where the lease for its `epoch` waits for it: in the
    /// cluster's own `system` (a store path) when there's one ready, on a `machine` of that size.
    async fn start(
        &self,
        session: Uuid,
        player: &str,
        epoch: i64,
        system: Option<&str>,
        machine: Resources,
    ) -> anyhow::Result<()>;

    /// Its default machine, where it has one of its own (a VM template's).
    fn machine(&self) -> Option<Resources> {
        None
    }

    /// The session `session` is deleted: remove what its players left here (none holds it).
    async fn forget(&self, session: Uuid) -> anyhow::Result<()>;

    /// Whether its players run the cluster's own system, given one to `start`.
    fn runs_systems(&self) -> bool {
        false
    }

    /// Build the cluster's players' system from its configuration's files (in `files`), as the
    /// build `id`, which tells control how it went with `token` (`POST /system/builds/{id}`).
    async fn build(&self, _id: Uuid, _token: &str, _files: &Path) -> anyhow::Result<()> {
        anyhow::bail!("this runner builds no systems")
    }

    /// The build `id` is over: clear away what's left of it.
    async fn end_build(&self, _id: Uuid) {}

    /// Say how the cluster's system goes, where whoever deploys it looks (besides control's log).
    async fn note(&self, _warning: bool, _reason: &str, _message: &str) {}

    /// Whether it keeps warm VMs ([`crate::pool`]).
    fn warms(&self) -> bool {
        false
    }

    /// The key of a warm VM on `machine` in `system` made now: one made with another isn't
    /// current.
    fn warm_key(&self, _machine: Resources, _system: Option<&str>) -> String {
        String::new()
    }

    /// Make a warm VM, `player`'s, on `machine` in `system`, its key `key`.
    async fn warm(
        &self,
        _player: &str,
        _machine: Resources,
        _system: Option<&str>,
        _key: &str,
    ) -> anyhow::Result<()> {
        anyhow::bail!("this runner keeps no warm VMs")
    }

    /// Its warm VMs (but those done).
    async fn warm_vms(&self) -> anyhow::Result<Vec<Warm>> {
        Ok(vec![])
    }

    /// The warm `player`'s VM is `session`'s, for its `epoch`th lease: no longer warm.
    async fn take(&self, _session: Uuid, _player: &str, _epoch: i64) -> anyhow::Result<()> {
        anyhow::bail!("this runner keeps no warm VMs")
    }

    /// Delete the warm `player`'s VM.
    async fn unwarm(&self, _player: &str) {}
}

/// A warm VM: its player, machine and key, and how long ago it was made.
#[derive(Debug, Clone)]
pub struct Warm {
    pub player: String,
    pub machine: Resources,
    pub key: String,
    pub age: Duration,
}

/// The environment a player gets: what it needs, and none of control's own (its Postgres).
pub(crate) const PASSED_ON: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "RUST_LOG",
    "CHLIVE_URL",
    "DIRIGENT_KOPIA_TOKEN",
    "ANTHROPIC_BASE_URL",
];

/// And what a build gets besides (nix/vm/build.nix): the cache, its key, and S3's.
const BUILD_PASSED_ON: &[&str] = &[
    "DIRIGENT_NIX_CACHE",
    "DIRIGENT_NIX_SIGNING_KEY",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
];

/// Players as processes on this machine, each session's in `<root>/<session>`: its workspace,
/// the player's state, and its output (`player.log`). They outlive a control that stops (and
/// resume their leases when it's back), as VMs would. They run on this machine's system, not the
/// cluster's own; that's built here too, with `build` (nix/vm/build.nix), into `<root>/builds`.
pub struct Local {
    pub id: String,
    pub root: PathBuf,
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Control's gRPC service for players.
    pub control_url: String,
    /// Control's HTTP API, which dirigents direct sessions through.
    pub control_http: String,
    /// What builds the cluster's system (`dirigent-build`), if it's built here.
    pub build: Option<PathBuf>,
}

#[async_trait::async_trait]
impl Runner for Local {
    fn id(&self) -> &str {
        &self.id
    }

    async fn start(
        &self,
        session: Uuid,
        player: &str,
        epoch: i64,
        _system: Option<&str>,
        _machine: Resources,
    ) -> anyhow::Result<()> {
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

    async fn build(&self, id: Uuid, token: &str, files: &Path) -> anyhow::Result<()> {
        let Some(program) = &self.build else {
            anyhow::bail!("no --player-build: the cluster's system isn't built here");
        };
        let dir = self.root.join("builds");
        std::fs::create_dir_all(&dir).with_context(|| format!("making {}", dir.display()))?;
        let log = std::fs::File::create(dir.join(format!("{id}.log")))
            .with_context(|| format!("making {}/{id}.log", dir.display()))?;
        let mut child = Command::new(program)
            .arg(files)
            .env_clear()
            .envs(
                PASSED_ON
                    .iter()
                    .chain(BUILD_PASSED_ON)
                    .filter_map(|k| Some((k, std::env::var_os(k)?))),
            )
            .env("DIRIGENT_BUILD", id.to_string())
            .env("DIRIGENT_BUILD_TOKEN", token)
            .env("DIRIGENT_CONTROL_HTTP", &self.control_http)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
            .with_context(|| format!("running {}", program.display()))?;
        info!(build = %id, "building the players' system");
        tokio::spawn(async move {
            if let Ok(status) = child.wait().await {
                info!(build = %id, %status, "the players' system's build exited");
            }
        });
        Ok(())
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
const LABEL_BUILD: &str = "dirigent.dev/build";
const LABEL_GENERATION: &str = "dirigent.dev/nix-generation";
/// A warm VM's: its key, until a session takes it.
const LABEL_WARM: &str = "dirigent.dev/warm";
/// A warm VM's machine.
const LABEL_CPU: &str = "dirigent.dev/cpu";
const LABEL_MEMORY: &str = "dirigent.dev/memory";
const LABEL_DISK: &str = "dirigent.dev/disk";

/// Players in KubeVirt VMs: a `VirtualMachineInstance` each, `player-<session>-<epoch>` (or
/// `warm-<player>`, booted before it's given a session: [`crate::pool`]), from a template
/// (Helm's): its image, disks, grace period and the Secrets it gets, its machine the session's. The VM
/// powers off when its player exits; [`Kube::clear`] deletes it then. The cluster's own system
/// is built in one too, `build-<id>`, from a template of its own (the same image, in build mode,
/// with the configuration's ConfigMap and the cache's signing key as disks).
pub struct Kube {
    id: String,
    /// Its VMs' `dirigent.dev/runner` label.
    name: String,
    vmis: Api<DynamicObject>,
    template: serde_json::Value,
    /// Its VMs' machine, unless a session asks for another.
    machine: Resources,
    builds: Option<Builds>,
    /// The Jobs that write the shared Nix store's generations, from a template (JSON; Helm's).
    writer: Option<(Api<Job>, serde_json::Value)>,
}

/// What [`Kube`] builds the cluster's system with.
struct Builds {
    template: serde_json::Value,
    /// The cache's public key (DIRIGENT_NIX_KEY), which players' VMs trust what's built with.
    key: String,
    events: Api<Event>,
    /// The configuration's ConfigMap, which the builds' Events are about.
    configmap: ObjectReference,
}

/// The Secret with the cache's signing key (DIRIGENT_NIX_SIGNING_KEY) and its public half
/// (DIRIGENT_NIX_KEY): `control ensure-secrets` makes it.
pub const NIX_SIGNING_SECRET: &str = "dirigent-nix-signing";

impl Kube {
    /// In `namespace` (the service account's when absent), from the VMI `template` (JSON); with
    /// `build` (a VMI template, and the configuration's ConfigMap), building the cluster's
    /// system; with `writer` (a Job template), writing the shared Nix store's generations.
    pub async fn new(
        name: &str,
        namespace: Option<&str>,
        template: &Path,
        build: Option<(&Path, &str)>,
        writer: Option<&Path>,
    ) -> anyhow::Result<Kube> {
        let template = read_template(template)?;
        // checked once, here: every VM is made from it
        vmi(&template, name, "player-", &[], "")?;
        let machine = template_machine(&template)?;
        sized(&template, machine)?;
        let client = kube::Client::try_default()
            .await
            .context("connecting to Kubernetes")?;
        let namespace = namespace.unwrap_or(client.default_namespace()).to_string();
        let vmis = Api::namespaced_with(client.clone(), &namespace, &vmi_resource());
        let writer = match writer {
            Some(path) => Some((
                Api::namespaced(client.clone(), &namespace),
                read_template(path)?,
            )),
            None => None,
        };
        let builds = match build {
            None => None,
            Some((path, configmap)) => {
                let template = read_template(path)?;
                vmi(&template, name, "build-", &[], "")?;
                let secrets: Api<Secret> = Api::namespaced(client.clone(), &namespace);
                let key =
                    crate::prune::secret_value(&secrets, NIX_SIGNING_SECRET, "DIRIGENT_NIX_KEY")
                        .await?;
                Some(Builds {
                    template,
                    key,
                    events: Api::namespaced(client, &namespace),
                    configmap: ObjectReference {
                        api_version: Some("v1".into()),
                        kind: Some("ConfigMap".into()),
                        name: Some(configmap.into()),
                        namespace: Some(namespace.clone()),
                        ..Default::default()
                    },
                })
            }
        };
        Ok(Kube {
            id: format!("kube:{namespace}/{name}"),
            name: name.to_string(),
            vmis,
            template,
            machine,
            builds,
            writer,
        })
    }

    /// Have generation `n` of the shared Nix store written (after `previous`, which it starts from):
    /// the Job `dirigent-nix-gen-<n>`, from the writer's template, told GENERATION and PREVIOUS.
    pub async fn start_writer(&self, n: u64, previous: Option<u64>) -> anyhow::Result<()> {
        let (jobs, template) = self.writer.as_ref().context("no writer's template")?;
        let mut job = template.clone();
        job["metadata"]["name"] = format!("dirigent-nix-gen-{n}").into();
        let container = &mut job["spec"]["template"]["spec"]["containers"][0];
        let env = container["env"]
            .as_array_mut()
            .context("the writer's template has no env")?;
        env.push(serde_json::json!({ "name": "GENERATION", "value": n.to_string() }));
        let previous = previous.map(|p| p.to_string()).unwrap_or_default();
        env.push(serde_json::json!({ "name": "PREVIOUS", "value": previous }));
        let job: Job = serde_json::from_value(job).context("the writer's template isn't a Job")?;
        match jobs.create(&PostParams::default(), &job).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(e)) if e.code == 409 => Ok(()),
            Err(e) => Err(e).context("making the writer's Job"),
        }
    }

    /// Whether generation `n`'s writer is done: none while it runs, then how it went.
    pub async fn writer_done(&self, n: u64) -> anyhow::Result<Option<Result<(), String>>> {
        let (jobs, _) = self.writer.as_ref().context("no writer's template")?;
        let Some(job) = jobs.get_opt(&format!("dirigent-nix-gen-{n}")).await? else {
            return Ok(Some(Err("its Job is gone".into())));
        };
        // done when it says so: a failed pod is tried again, up to its backoffLimit
        let finished = |kind: &str| {
            job.status
                .as_ref()
                .and_then(|s| s.conditions.as_ref())
                .is_some_and(|c| c.iter().any(|c| c.type_ == kind && c.status == "True"))
        };
        Ok(if finished("Complete") {
            Some(Ok(()))
        } else if finished("Failed") {
            Some(Err("its Job failed".into()))
        } else {
            None
        })
    }

    /// Generation `n`'s writer is over: its Job goes (with its pods).
    pub async fn end_writer(&self, n: u64) {
        let Some((jobs, _)) = &self.writer else {
            return;
        };
        let gone = DeleteParams::background();
        if let Err(e) = jobs.delete(&format!("dirigent-nix-gen-{n}"), &gone).await {
            warn!(generation = n, "deleting the writer's Job: {e}");
        }
    }

    /// The shared Nix store's generations this runner's VMs have.
    pub async fn generations_used(&self) -> anyhow::Result<HashSet<u64>> {
        let mine = ListParams::default()
            .labels(&format!("{LABEL_RUNNER}={},{LABEL_GENERATION}", self.name));
        Ok(self
            .vmis
            .list(&mine)
            .await?
            .into_iter()
            .filter_map(|v| v.metadata.labels?.get(LABEL_GENERATION)?.parse().ok())
            .collect())
    }

    /// Every `every`, delete this runner's VMs that are done (powered off, or failed), and those
    /// past their `startup` whose player holds no lease (it never came, or lost it). A build's
    /// is left until it's done: control ends it ([`Runner::end_build`]); a warm one is the
    /// pool's.
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
            let labels = vmi.metadata.labels.as_ref();
            // a build's, or a warm one (the pool's to keep or delete)
            let aside =
                labels.is_some_and(|l| l.contains_key(LABEL_BUILD) || l.contains_key(LABEL_WARM));
            let player = labels
                .and_then(|l| l.get(LABEL_PLAYER))
                .map(|p| player_subject(p));
            let old = vmi
                .metadata
                .creation_timestamp
                .as_ref()
                .is_some_and(|t| age(t) > startup);
            let why = match phase {
                "Succeeded" | "Failed" => phase,
                _ if aside => continue,
                _ if old && !player.is_some_and(|p| held.contains(&p)) => "holds no lease",
                _ => continue,
            };
            info!(vmi = name, why, "deleting a player's VM");
            self.delete(&name).await;
        }
        Ok(())
    }

    /// A player's VM's lines for Nix (into `env`): the cache's key (with builds), the cluster's
    /// `system`, and the shared Nix store's current generation, which it keeps until it's gone
    /// (and which it's then labelled with).
    fn nix(&self, env: &mut String, system: Option<&str>) -> Option<String> {
        if let Some(builds) = &self.builds {
            env.push_str(&format!("DIRIGENT_NIX_KEY={}\n", builds.key));
        }
        if let Some(system) = system {
            env.push_str(&format!("DIRIGENT_SYSTEM={system}\n"));
        }
        let generation = crate::nixstore::current().map(|n| n.to_string());
        if let Some(n) = &generation {
            env.push_str(&format!("DIRIGENT_NIX_GENERATION={n}\n"));
        }
        generation
    }

    async fn delete(&self, name: &str) {
        match self.vmis.delete(name, &DeleteParams::default()).await {
            Ok(_) => {}
            Err(kube::Error::Api(e)) if e.code == 404 => {}
            Err(e) => warn!(vmi = name, "deleting: {e}"),
        }
    }

    async fn create(&self, vmi: &DynamicObject) -> anyhow::Result<()> {
        let name = vmi.metadata.name.clone().unwrap_or_default();
        match self.vmis.create(&PostParams::default(), vmi).await {
            Ok(_) => Ok(()),
            // made by an earlier try
            Err(kube::Error::Api(e)) if e.code == 409 => Ok(()),
            Err(e) => Err(e).with_context(|| format!("making the VM {name}")),
        }
    }
}

fn read_template(path: &Path) -> anyhow::Result<serde_json::Value> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading the VM template {}", path.display()))?;
    serde_json::from_str(&text)
        .with_context(|| format!("{}: not a JSON VirtualMachineInstance", path.display()))
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

    fn machine(&self) -> Option<Resources> {
        Some(self.machine)
    }

    fn runs_systems(&self) -> bool {
        self.builds.is_some()
    }

    async fn start(
        &self,
        session: Uuid,
        player: &str,
        epoch: i64,
        system: Option<&str>,
        machine: Resources,
    ) -> anyhow::Result<()> {
        let mut env = format!(
            "DIRIGENT_SESSION={session}\nDIRIGENT_PLAYER_ID={player}\nDIRIGENT_EPOCH={epoch}\n"
        );
        let generation = self.nix(&mut env, system);
        let (session_label, epoch_label) = (session.to_string(), epoch.to_string());
        let mut labels = vec![
            (LABEL_SESSION, session_label.as_str()),
            (LABEL_PLAYER, player),
            (LABEL_EPOCH, epoch_label.as_str()),
        ];
        if let Some(n) = &generation {
            labels.push((LABEL_GENERATION, n));
        }
        let vmi = vmi(
            &sized(&self.template, machine)?,
            &self.name,
            &format!("player-{session}-{epoch}"),
            &labels,
            &env,
        )?;
        self.create(&vmi).await?;
        let name = vmi.metadata.name.unwrap_or_default();
        info!(session = %session, player, epoch, vmi = name, system, ?machine, "player started");
        Ok(())
    }

    fn warms(&self) -> bool {
        true
    }

    fn warm_key(&self, machine: Resources, system: Option<&str>) -> String {
        use sha2::{Digest, Sha256};
        let mut sha = Sha256::new();
        sha.update(self.template.to_string());
        sha.update(format!(
            "{}/{}/{}",
            machine.cpu, machine.memory, machine.disk
        ));
        sha.update(system.unwrap_or("stock"));
        let generation = crate::nixstore::current().unwrap_or(0);
        sha.update(generation.to_string());
        sha.finalize()[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    async fn warm(
        &self,
        player: &str,
        machine: Resources,
        system: Option<&str>,
        key: &str,
    ) -> anyhow::Result<()> {
        let mut env = format!("DIRIGENT_WARM=true\nDIRIGENT_PLAYER_ID={player}\n");
        let generation = self.nix(&mut env, system);
        let (cpu, memory, disk) = (
            machine.cpu.to_string(),
            machine.memory.to_string(),
            machine.disk.to_string(),
        );
        let mut labels = vec![
            (LABEL_WARM, key),
            (LABEL_PLAYER, player),
            (LABEL_CPU, cpu.as_str()),
            (LABEL_MEMORY, memory.as_str()),
            (LABEL_DISK, disk.as_str()),
        ];
        if let Some(n) = &generation {
            labels.push((LABEL_GENERATION, n));
        }
        let vmi = vmi(
            &sized(&self.template, machine)?,
            &self.name,
            &format!("warm-{player}"),
            &labels,
            &env,
        )?;
        self.create(&vmi).await
    }

    async fn warm_vms(&self) -> anyhow::Result<Vec<Warm>> {
        let warm =
            ListParams::default().labels(&format!("{LABEL_RUNNER}={},{LABEL_WARM}", self.name));
        let mut found = Vec::new();
        for vmi in self.vmis.list(&warm).await? {
            if matches!(
                vmi.data["status"]["phase"].as_str(),
                Some("Succeeded" | "Failed")
            ) {
                continue;
            }
            let Some(labels) = vmi.metadata.labels.as_ref() else {
                continue;
            };
            let n = |l: &str| labels.get(l).and_then(|v| v.parse().ok());
            let (Some(player), Some(key), Some(cpu), Some(memory), Some(disk)) = (
                labels.get(LABEL_PLAYER),
                labels.get(LABEL_WARM),
                n(LABEL_CPU),
                n(LABEL_MEMORY),
                n(LABEL_DISK),
            ) else {
                continue;
            };
            let age = vmi.metadata.creation_timestamp.as_ref().map(age);
            found.push(Warm {
                player: player.clone(),
                machine: Resources { cpu, memory, disk },
                key: key.clone(),
                age: age.unwrap_or_default(),
            });
        }
        Ok(found)
    }

    async fn take(&self, session: Uuid, player: &str, epoch: i64) -> anyhow::Result<()> {
        let labels = serde_json::json!({ "metadata": { "labels": {
            LABEL_WARM: null,
            LABEL_SESSION: session.to_string(),
            LABEL_EPOCH: epoch.to_string(),
        } } });
        let name = format!("warm-{player}");
        self.vmis
            .patch(&name, &PatchParams::default(), &Patch::Merge(&labels))
            .await
            .with_context(|| format!("giving the warm VM {name} to {session}"))?;
        info!(session = %session, player, epoch, vmi = name, "a warm VM took the session");
        Ok(())
    }

    async fn unwarm(&self, player: &str) {
        self.delete(&format!("warm-{player}")).await;
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

    async fn build(&self, id: Uuid, token: &str, _files: &Path) -> anyhow::Result<()> {
        let Some(builds) = &self.builds else {
            anyhow::bail!("no --kube-build-template: the cluster's system isn't built here");
        };
        let env = format!(
            "DIRIGENT_BUILD={id}\nDIRIGENT_BUILD_TOKEN={token}\nDIRIGENT_NIX_KEY={}\n",
            builds.key
        );
        let id_label = id.to_string();
        let vmi = vmi(
            &builds.template,
            &self.name,
            &format!("build-{id}"),
            &[(LABEL_BUILD, &id_label)],
            &env,
        )?;
        self.create(&vmi).await?;
        info!(build = %id, "building the players' system");
        Ok(())
    }

    async fn end_build(&self, id: Uuid) {
        self.delete(&format!("build-{id}")).await;
    }

    async fn note(&self, warning: bool, reason: &str, message: &str) {
        let Some(builds) = &self.builds else {
            return;
        };
        let now = Some(Time(k8s_openapi::jiff::Timestamp::now()));
        let event = Event {
            metadata: ObjectMeta {
                generate_name: builds.configmap.name.as_ref().map(|n| format!("{n}.")),
                ..Default::default()
            },
            involved_object: builds.configmap.clone(),
            reason: Some(reason.into()),
            // Events are short; Nix's errors aren't
            message: Some(message.chars().take(1000).collect()),
            type_: Some(if warning { "Warning" } else { "Normal" }.into()),
            first_timestamp: now.clone(),
            last_timestamp: now,
            count: Some(1),
            source: Some(k8s_openapi::api::core::v1::EventSource {
                component: Some("dirigent-control".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        if let Err(e) = builds.events.create(&PostParams::default(), &event).await {
            warn!(reason, "posting an Event: {e}");
        }
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

/// The volume of a player's VM template that's its workspace disk (an `emptyDisk`).
pub const WORK_VOLUME: &str = "work";

/// The machine a VM from `template` has: its cores, guest memory and workspace disk.
fn template_machine(template: &serde_json::Value) -> anyhow::Result<Resources> {
    let domain = &template["spec"]["domain"];
    let cpu = domain["cpu"]["cores"]
        .as_u64()
        .context("the VM template has no spec.domain.cpu.cores")?;
    let memory = domain["memory"]["guest"]
        .as_str()
        .context("the VM template has no spec.domain.memory.guest")?;
    let disk = template["spec"]["volumes"]
        .as_array()
        .and_then(|v| v.iter().find(|v| v["name"] == WORK_VOLUME))
        .and_then(|v| v["emptyDisk"]["capacity"].as_str())
        .with_context(|| {
            format!("the VM template has no volume {WORK_VOLUME:?} with an emptyDisk")
        })?;
    Ok(Resources {
        cpu: cpu.try_into().context("the VM template's cores")?,
        memory: crate::machines::gib(memory).context("the VM template's memory")?,
        disk: crate::machines::gib(disk).context("the VM template's workspace disk")?,
    })
}

/// `template`, its VM on `machine`.
fn sized(template: &serde_json::Value, machine: Resources) -> anyhow::Result<serde_json::Value> {
    let mut t = template.clone();
    t["spec"]["domain"]["cpu"]["cores"] = machine.cpu.into();
    t["spec"]["domain"]["memory"]["guest"] = format!("{}Gi", machine.memory).into();
    let work = t["spec"]["volumes"]
        .as_array_mut()
        .and_then(|v| v.iter_mut().find(|v| v["name"] == WORK_VOLUME))
        .with_context(|| format!("the VM template has no volume {WORK_VOLUME:?}"))?;
    work["emptyDisk"]["capacity"] = format!("{}Gi", machine.disk).into();
    Ok(t)
}

/// The VM `name` from `template`: labelled with `labels` (and the runner's), its config
/// volume's user data with `env` (lines) after the template's.
fn vmi(
    template: &serde_json::Value,
    runner: &str,
    name: &str,
    labels: &[(&str, &str)],
    env: &str,
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
    let mut data = config["userData"].as_str().unwrap_or_default().to_string();
    if !data.is_empty() && !data.ends_with('\n') {
        data.push('\n');
    }
    data.push_str(env);
    config["userData"] = data.into();
    let meta = &mut vmi["metadata"];
    if !meta.is_object() {
        *meta = serde_json::json!({});
    }
    meta["name"] = name.into();
    let all = &mut meta["labels"];
    if !all.is_object() {
        *all = serde_json::json!({});
    }
    for (k, v) in labels {
        all[*k] = (*v).into();
    }
    all[LABEL_RUNNER] = runner.into();
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
                "domain": { "cpu": { "cores": 2 }, "memory": { "guest": "4Gi" } },
                "volumes": [
                    { "name": "disk", "containerDisk": { "image": "player-vm" } },
                    { "name": CONFIG_VOLUME, "cloudInitNoCloud": {
                        "userData": "DIRIGENT_CONTROL_URL=http://control:8091"
                    } },
                    { "name": WORK_VOLUME, "emptyDisk": { "capacity": "20Gi" } },
                ],
            },
        })
    }

    #[test]
    fn a_vm_is_on_the_templates_machine_or_the_one_asked_for() {
        let t = template();
        let default = template_machine(&t).unwrap();
        assert_eq!(
            default,
            Resources {
                cpu: 2,
                memory: 4,
                disk: 20
            }
        );
        let big = Resources {
            cpu: 8,
            memory: 32,
            disk: 100,
        };
        let sized = sized(&t, big).unwrap();
        assert_eq!(template_machine(&sized).unwrap(), big);
        assert_eq!(sized["spec"]["domain"]["memory"]["guest"], "32Gi");
        assert_eq!(
            sized["spec"]["volumes"][2]["emptyDisk"]["capacity"],
            "100Gi"
        );
        let mut odd = template();
        odd["spec"]["domain"]["memory"]["guest"] = "1536Mi".into();
        assert!(template_machine(&odd).is_err());
    }

    #[test]
    fn a_vm_is_the_template_named_and_labelled_with_its_environment() {
        let vmi = vmi(
            &template(),
            "dirigent",
            "player-s-3",
            &[(LABEL_SESSION, "s"), (LABEL_EPOCH, "3")],
            "DIRIGENT_SESSION=s\nDIRIGENT_EPOCH=3\n",
        )
        .unwrap();
        assert_eq!(vmi.metadata.name.as_deref(), Some("player-s-3"));
        let labels = vmi.metadata.labels.unwrap();
        assert_eq!(labels[LABEL_SESSION], "s");
        assert_eq!(labels[LABEL_EPOCH], "3");
        assert_eq!(labels[LABEL_RUNNER], "dirigent");
        assert_eq!(labels["app.kubernetes.io/name"], "dirigent-player");
        assert_eq!(vmi.data["spec"]["terminationGracePeriodSeconds"], 3600);
        assert_eq!(
            vmi.data["spec"]["volumes"][1]["cloudInitNoCloud"]["userData"],
            "DIRIGENT_CONTROL_URL=http://control:8091\nDIRIGENT_SESSION=s\nDIRIGENT_EPOCH=3\n"
        );
    }

    #[test]
    fn a_template_without_the_config_volume_is_refused() {
        let mut t = template();
        t["spec"]["volumes"]
            .as_array_mut()
            .unwrap()
            .retain(|v| v["name"] != CONFIG_VOLUME);
        let e = vmi(&t, "d", "v", &[], "").unwrap_err();
        assert!(e.to_string().contains("no volume"), "{e}");
        let mut t = template();
        t["kind"] = "VirtualMachine".into();
        assert!(vmi(&t, "d", "v", &[], "").is_err());
    }
}
