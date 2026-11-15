//! `dirigent host`: this machine as a place where your sessions run. The daemon (`host run`)
//! connects to control as you, says what the machine is, and runs a player for each session
//! control starts here (a session you placed on this host), each in a directory of its own
//! under the host's roots: `<root>/<session>/{workspace,state}`. It's there for as long as it's
//! connected; control keeps nothing of it. Players run as this binary (`dirigent __player`),
//! each in a process of its own that outlives the daemon: it holds its session's lease, not the
//! daemon. `host install` makes the daemon a service of the system's ([`service`]).

pub mod service;
pub mod status;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use dirigent_protocol::control::pb;
use dirigent_protocol::control::pb::players_client::PlayersClient;
use serde::Deserialize;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{info, warn};

use crate::api::Api;

/// The longest wait before connecting again.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// What the host is and where it runs players, from `host.toml`.
#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// dirigent's address, the web's (`https://dirigent.example.com`): its API is at `/api`,
    /// chlive at `/live`, and control's players' service at the address itself. By default, the
    /// signed-in API's.
    pub url: Option<String>,
    /// Control's API, chlive and the players' service, where they aren't at `url` (control's
    /// own API, signed in with `login --control`; the players' service on a tailnet).
    pub api: Option<String>,
    pub chlive: Option<String>,
    pub players: Option<String>,
    /// The host's name, how sessions are placed on it: by default, this machine's.
    pub name: Option<String>,
    /// Where sessions' directories go; by default `~/dirigent`.
    pub roots: Vec<PathBuf>,
    /// How many players it runs at once (2 by default).
    pub slots: Option<u32>,
    /// Words for what it has (e.g. `xcode`), shown with it.
    pub labels: Vec<String>,
    /// What the players' system prompt says of it, besides what's found (its system, user).
    pub environment: Option<String>,
    /// More of the players' arguments (see `dirigent __player --help`).
    pub player_args: Vec<String>,
}

/// Where the host reaches dirigent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub api: String,
    pub chlive: String,
    pub players: String,
}

impl Config {
    /// The config at `path`; none there, the defaults.
    pub fn load(path: &Path) -> anyhow::Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("reading {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Its endpoints, `signed_in` the API the CLI is signed in to.
    pub fn endpoints(&self, signed_in: &str) -> anyhow::Result<Endpoints> {
        let origin = match &self.url {
            Some(url) => Some(url.trim_end_matches('/').to_string()),
            None => signed_in.strip_suffix("/api").map(str::to_string),
        };
        let from = |what: &str, given: &Option<String>, derived: Option<String>| {
            given.clone().or(derived).with_context(|| {
                format!("the host's {what} isn't known: give `url` (or `{what}`) in its config")
            })
        };
        let api = from(
            "api",
            &self.api,
            origin.as_ref().map(|o| format!("{o}/api")),
        )
        .or_else(|_| Ok::<_, anyhow::Error>(signed_in.to_string()))?;
        let live = origin.as_deref().map(|o| {
            let ws = o
                .replacen("https://", "wss://", 1)
                .replacen("http://", "ws://", 1);
            format!("{ws}/live")
        });
        Ok(Endpoints {
            api,
            chlive: from("chlive", &self.chlive, live)?,
            players: from("players", &self.players, origin)?,
        })
    }

    pub fn slots(&self) -> u32 {
        self.slots.unwrap_or(2).max(1)
    }

    /// The roots, `~` expanded; by default `~/dirigent`.
    pub fn roots(&self) -> Vec<PathBuf> {
        if self.roots.is_empty() {
            return vec![expand(Path::new("~/dirigent"))];
        }
        self.roots.iter().map(|r| expand(r)).collect()
    }
}

/// `path` with a leading `~` as the home directory.
fn expand(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => path.to_path_buf(),
    }
}

/// The config's path: `$DIRIGENT_HOST_CONFIG`, or `dirigent/host.toml` beside the credentials.
pub fn config_path() -> anyhow::Result<PathBuf> {
    if let Some(path) = std::env::var_os("DIRIGENT_HOST_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    let creds = crate::credentials::path()?;
    Ok(creds.with_file_name("host.toml"))
}

/// The host's name, how sessions are placed on it: the config's, or this machine's.
pub async fn name(config: &Config) -> String {
    if let Some(name) = &config.name {
        return name.clone();
    }
    match service::output("hostname", &["-s"]).await {
        Ok(name) if !name.trim().is_empty() => name.trim().to_string(),
        _ => "host".into(),
    }
}

/// The machine, as the host says it is.
struct Machine {
    name: String,
    os: &'static str,
    arch: &'static str,
    /// Its system's name and version (`macOS 15.2`, `Ubuntu 24.04.1 LTS`).
    system: String,
    cpus: u32,
    memory: u32,
    user: String,
}

impl Machine {
    async fn here(config: &Config) -> Machine {
        // `macos`, `linux`: as the protocol says it
        let os = std::env::consts::OS;
        let output = |cmd: &str, args: &[&str]| {
            let mut c = tokio::process::Command::new(cmd);
            c.args(args).stdin(Stdio::null()).stderr(Stdio::null());
            async move {
                let out = c.output().await.ok()?;
                let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
                (out.status.success() && !text.is_empty()).then_some(text)
            }
        };
        let (system, memory) = match os {
            "macos" => (
                output("sw_vers", &["-productVersion"])
                    .await
                    .map_or("macOS".into(), |v| format!("macOS {v}")),
                output("sysctl", &["-n", "hw.memsize"])
                    .await
                    .and_then(|b| b.parse::<u64>().ok())
                    .unwrap_or(0),
            ),
            _ => (
                std::fs::read_to_string("/etc/os-release")
                    .ok()
                    .and_then(|r| {
                        r.lines()
                            .find_map(|l| l.strip_prefix("PRETTY_NAME="))
                            .map(|n| n.trim_matches('"').to_string())
                    })
                    .unwrap_or_else(|| "Linux".into()),
                std::fs::read_to_string("/proc/meminfo")
                    .ok()
                    .and_then(|m| {
                        let kib = m.lines().find_map(|l| l.strip_prefix("MemTotal:"))?;
                        kib.trim().trim_end_matches("kB").trim().parse::<u64>().ok()
                    })
                    .map_or(0, |kib| kib * 1024),
            ),
        };
        let name = name(config).await;
        Machine {
            name,
            os,
            arch: std::env::consts::ARCH,
            system,
            cpus: std::thread::available_parallelism().map_or(1, |n| n.get() as u32),
            memory: (memory >> 30) as u32,
            user: std::env::var("USER").unwrap_or_else(|_| "root".into()),
        }
    }

    /// Its one-line name in the sessions' logs.
    fn label(&self) -> String {
        format!("{} ({}, {})", self.name, self.system, self.arch)
    }

    /// What the players' system prompt says of it.
    fn environment(&self, config: &Config) -> String {
        let mut text = format!(
            "- You run as `{}` on {}, a {} machine of the user's ({}), with network access.\n\
             - Tools are what this machine has installed: check with `command -v` before relying \
             on one. Don't install software system-wide unless the task asks for it; use the \
             project's own tooling, or a virtual environment in the project.\n",
            self.user, self.name, self.system, self.arch
        );
        if let Some(more) = config.environment.as_deref().map(str::trim) {
            if !more.is_empty() {
                text.push_str(&format!("- {more}\n"));
            }
        }
        text
    }
}

/// The host's players: running, by session.
#[derive(Default)]
struct Players(Mutex<HashMap<String, u32>>);

impl Players {
    fn running(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

/// Run the host until it's stopped: connected to control, and again when the connection breaks.
pub async fn run(mut api: Api, config: Config) -> anyhow::Result<()> {
    let endpoints = config.endpoints(&api.creds.api)?;
    let roots = config.roots();
    for root in &roots {
        std::fs::create_dir_all(root).with_context(|| format!("making {}", root.display()))?;
    }
    let machine = Machine::here(&config).await;
    info!(
        name = machine.name,
        system = machine.system,
        players = endpoints.players,
        "host starting"
    );
    let players: Arc<Players> = Default::default();
    let mut backoff = Duration::from_millis(500);
    loop {
        let connected = connect(&mut api, &config, &endpoints, &roots, &machine, &players).await;
        match connected {
            Ok(()) => {
                warn!("control ended the call: connecting again");
                backoff = Duration::from_millis(500);
            }
            Err(e) => warn!("connecting to control at {}: {e:#}", endpoints.players),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// One connection, until it ends.
async fn connect(
    api: &mut Api,
    config: &Config,
    endpoints: &Endpoints,
    roots: &[PathBuf],
    machine: &Machine,
    players: &Arc<Players>,
) -> anyhow::Result<()> {
    let channel = dirigent_player::control_channel(&endpoints.players).await?;
    let hello = pb::Hello {
        name: machine.name.clone(),
        os: machine.os.into(),
        arch: machine.arch.into(),
        cpus: machine.cpus,
        memory: machine.memory,
        labels: config.labels.clone(),
        slots: config.slots(),
        version: dirigent_player::VERSION.into(),
        sessions: sessions(roots),
    };
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    tx.send(pb::FromHost {
        msg: Some(pb::from_host::Msg::Hello(hello)),
    })
    .await
    .expect("the receiver is here");
    let mut call = tonic::Request::new(ReceiverStream::new(rx));
    let (header, value) = api.caller().await?;
    call.metadata_mut().insert(header, value.parse()?);
    let mut inbound = PlayersClient::new(channel)
        .host(call)
        .await
        .map_err(|s| anyhow::anyhow!("{}", s.message()))?
        .into_inner();
    info!("connected");
    while let Some(msg) = inbound.message().await? {
        match msg.msg {
            Some(pb::to_host::Msg::Start(start)) => {
                let answer = match players.running() < config.slots() as usize {
                    true => spawn(&start, config, endpoints, roots, machine, players)
                        .map_err(|e| format!("{e:#}")),
                    false => Err(format!("the host is full ({} players)", config.slots())),
                };
                let msg = match answer {
                    Ok(()) => pb::from_host::Msg::Started(pb::Started {
                        session: start.session,
                        player: start.player,
                    }),
                    Err(error) => {
                        warn!(session = start.session, error, "couldn't start a player");
                        pb::from_host::Msg::StartFailed(pb::StartFailed {
                            session: start.session,
                            player: start.player,
                            error,
                        })
                    }
                };
                let _ = tx.send(pb::FromHost { msg: Some(msg) }).await;
            }
            Some(pb::to_host::Msg::Forget(forget)) => {
                forget_session(&forget.session, roots, players)
            }
            None => {}
        }
    }
    Ok(())
}

/// The sessions whose directories the roots have.
fn sessions(roots: &[PathBuf]) -> Vec<String> {
    workspaces(roots).into_iter().map(|(id, _)| id).collect()
}

/// The sessions' directories under `roots`: by session.
pub fn workspaces(roots: &[PathBuf]) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_uuid(&name) && entry.path().is_dir() {
                found.push((name, entry.path()));
            }
        }
    }
    found.sort();
    found
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36
        && s.chars().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The session's directory: where it is already, or in the first root.
fn directory(session: &str, roots: &[PathBuf]) -> PathBuf {
    roots
        .iter()
        .map(|r| r.join(session))
        .find(|d| d.is_dir())
        .unwrap_or_else(|| roots[0].join(session))
}

/// What a player's environment keeps of the host's.
const PASSED_ON: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "LANG",
    "LC_ALL",
    "TZ",
    "RUST_LOG",
    "ANTHROPIC_BASE_URL",
    "OPENAI_BASE_URL",
];

/// Start the player `start` asks for, in its session's directory.
fn spawn(
    start: &pb::Start,
    config: &Config,
    endpoints: &Endpoints,
    roots: &[PathBuf],
    machine: &Machine,
    players: &Arc<Players>,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        is_uuid(&start.session),
        "{:?} isn't a session",
        start.session
    );
    let dir = directory(&start.session, roots);
    for sub in ["workspace", "state"] {
        std::fs::create_dir_all(dir.join(sub))
            .with_context(|| format!("making {}", dir.join(sub).display()))?;
    }
    let environment = dir.join("environment.md");
    std::fs::write(&environment, machine.environment(config))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("player.log"))
        .with_context(|| format!("opening {}/player.log", dir.display()))?;
    let me = std::env::current_exe().context("finding this binary")?;
    let mut cmd = tokio::process::Command::new(&me);
    cmd.arg(crate::PLAYER)
        .args(["--session", &start.session])
        .args(["--player-id", &start.player])
        .args(["--epoch", &start.epoch.to_string()])
        .args(["--control-url", &endpoints.players])
        .args(["--control-http", &endpoints.api])
        .args(["--chlive-url", &endpoints.chlive])
        .arg("--workdir")
        .arg(dir.join("workspace"))
        .arg("--state-dir")
        .arg(dir.join("state"))
        .arg("--environment")
        .arg(&environment)
        .args(["--environment-name", &machine.label()])
        .args(&config.player_args)
        .env_clear()
        .envs(
            PASSED_ON
                .iter()
                .filter_map(|k| Some((k, std::env::var_os(k)?))),
        )
        // in its environment, not its command line, which others may see
        .env("DIRIGENT_LEASE_TOKEN", &start.token)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        // its own process group: stopping the daemon leaves it running, holding its lease
        .process_group(0);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("running {}", me.display()))?;
    let pid = child.id().unwrap_or_default();
    info!(
        session = start.session,
        player = start.player,
        pid,
        "player started"
    );
    let session = start.session.clone();
    players.0.lock().unwrap().insert(session.clone(), pid);
    let players = players.clone();
    tokio::spawn(async move {
        let status = child.wait().await;
        players.0.lock().unwrap().remove(&session);
        match status {
            Ok(s) if s.success() => info!(session, "player exited"),
            Ok(s) => warn!(session, status = %s, "player failed"),
            Err(e) => warn!(session, "waiting for the player: {e}"),
        }
    });
    Ok(())
}

/// Remove what the host has of `session`, unless its player runs here still.
fn forget_session(session: &str, roots: &[PathBuf], players: &Players) {
    if !is_uuid(session) || players.0.lock().unwrap().contains_key(session) {
        return;
    }
    for root in roots {
        let dir = root.join(session);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => info!(session, "forgot the session"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!(session, "removing {}: {e}", dir.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_config_reads_with_defaults_and_refuses_what_it_doesnt_know() {
        let c: Config = toml::from_str(
            r#"
            url = "https://dirigent.example.com/"
            name = "mac"
            roots = ["~/d", "/srv/d"]
            labels = ["xcode"]
            environment = "Xcode 16 is installed."
            "#,
        )
        .unwrap();
        assert_eq!(c.name.as_deref(), Some("mac"));
        assert_eq!(c.slots(), 2);
        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            c.roots(),
            [PathBuf::from(format!("{home}/d")), PathBuf::from("/srv/d")]
        );
        assert_eq!(
            Config::default().roots(),
            [PathBuf::from(format!("{home}/dirigent"))]
        );
        assert!(toml::from_str::<Config>("rooots = []").is_err());
    }

    #[test]
    fn the_endpoints_come_from_the_url_or_the_signed_in_api() {
        let c: Config = toml::from_str(r#"url = "https://d.example.com""#).unwrap();
        assert_eq!(
            c.endpoints("https://other/api").unwrap(),
            Endpoints {
                api: "https://d.example.com/api".into(),
                chlive: "wss://d.example.com/live".into(),
                players: "https://d.example.com".into(),
            }
        );
        let signed_in = Config::default()
            .endpoints("http://localhost:3000/api")
            .unwrap();
        assert_eq!(signed_in.chlive, "ws://localhost:3000/live");
        assert_eq!(signed_in.players, "http://localhost:3000");
        // control's own API: the rest must be given
        assert!(Config::default()
            .endpoints("http://127.0.0.1:8090")
            .is_err());
        let c: Config = toml::from_str(
            r#"
            chlive = "ws://127.0.0.1:8080/live"
            players = "http://127.0.0.1:8091"
            "#,
        )
        .unwrap();
        let direct = c.endpoints("http://127.0.0.1:8090").unwrap();
        assert_eq!(direct.api, "http://127.0.0.1:8090");
        assert_eq!(direct.players, "http://127.0.0.1:8091");
    }

    #[test]
    fn a_hosts_directories_are_its_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let s = "6f1c1a52-4a32-4c47-9b0f-0f1e3c7d9a10";
        std::fs::create_dir_all(tmp.path().join(s)).unwrap();
        std::fs::create_dir_all(tmp.path().join("other")).unwrap();
        let roots = vec![tmp.path().to_path_buf()];
        assert_eq!(sessions(&roots), [s]);
        forget_session(s, &roots, &Players::default());
        assert!(sessions(&roots).is_empty() && tmp.path().join("other").exists());
        forget_session("../other", &roots, &Players::default());
        assert!(tmp.path().join("other").exists());
    }

    #[test]
    fn the_environment_says_what_the_machine_is() {
        let m = Machine {
            name: "mac".into(),
            os: "macos",
            arch: "aarch64",
            system: "macOS 15.2".into(),
            cpus: 10,
            memory: 32,
            user: "adam".into(),
        };
        let config = Config {
            environment: Some("Xcode 16 is installed.".into()),
            ..Config::default()
        };
        let text = m.environment(&config);
        assert!(text.starts_with("- You run as `adam` on mac, a macOS 15.2 machine"));
        assert!(text.ends_with("- Xcode 16 is installed.\n"));
        assert_eq!(m.label(), "mac (macOS 15.2, aarch64)");
    }
}
