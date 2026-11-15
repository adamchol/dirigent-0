//! `dirigent host status`: what of the host runs here, and where. The service (whichever
//! installed it: `host install`, Homebrew, home-manager; NixOS's system unit), as launchd or
//! systemd tells it; whether control has the host connected; the players running (from the
//! process list: each is `dirigent __player --session …`), and the sessions' directories kept
//! under the host's roots; the last warnings and errors of its log. Nothing of it is kept: it's
//! all read from what's there.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::service::{self, Os, LABEL, UNIT};
use super::Config;
use crate::api::Api;

/// The labels the host's launchd agent has, by what installed it.
const AGENTS: [(&str, &str); 3] = [
    (LABEL, "dirigent host install"),
    ("org.nix-community.home.dirigent-host", "home-manager"),
    ("homebrew.mxcl.dirigent", "Homebrew"),
];
/// The system daemons earlier versions installed (`host install --system`, nix-darwin), which
/// can't reach your login session: to remove.
const OLD_DAEMONS: [&str; 2] = ["org.nixos.dirigent-host", LABEL];
/// How many of the log's last warnings and errors it shows.
const LOG_LINES: usize = 5;

#[derive(Debug, Serialize, PartialEq)]
pub struct Status {
    pub services: Vec<Service>,
    pub connection: Connection,
    pub slots: u32,
    pub running: Vec<Running>,
    /// Sessions' directories with no player running in them.
    pub kept: Vec<Kept>,
    /// The log's last warnings and errors.
    pub log: Vec<String>,
}

/// One of the host's services, as launchd or systemd tells it.
#[derive(Debug, Default, Serialize, PartialEq)]
pub struct Service {
    /// `launchd agent`, `launchd daemon`, `systemd user unit`, `systemd system unit`.
    pub kind: String,
    /// Its label or unit.
    pub name: String,
    /// What installed it, where that's known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    pub file: Option<String>,
    /// `running`, `not running`, launchd's or systemd's own word.
    pub state: String,
    pub pid: Option<u32>,
    pub last_exit: Option<String>,
    /// Who it runs as, where it isn't you.
    pub user: Option<String>,
    pub log: Option<String>,
    /// A system daemon of an earlier version's, to remove.
    pub old: bool,
    #[serde(skip)]
    env: HashMap<String, String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Connection {
    /// The host's name, how sessions are placed on it.
    pub name: String,
    /// Whether control has it connected (none: unknown, `error` says why).
    pub connected: Option<bool>,
    pub version: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Running {
    pub session: String,
    pub pid: u32,
    pub user: String,
    /// How long it's been running, as `ps` says (`[[dd-]hh:]mm:ss`).
    pub elapsed: String,
    pub workdir: Option<String>,
    pub repo: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Kept {
    pub session: String,
    pub dir: String,
    pub repo: Option<String>,
}

/// Find it all out.
pub async fn status(http: reqwest::Client) -> anyhow::Result<Status> {
    let os = service::os()?;
    let services = services(os).await;
    // the config the running service reads (NixOS's is in /etc), else the CLI's
    let config_path = services
        .iter()
        .find_map(|s| s.env.get("DIRIGENT_HOST_CONFIG").map(PathBuf::from))
        .map_or_else(super::config_path, Ok)?;
    let config = Config::load(&config_path)?;
    let name = super::name(&config).await;
    let (connection, repos) = from_control(http, &name).await;
    let ps = service::output("ps", &["-ax", "-o", "pid=,user=,etime=,args="])
        .await
        .unwrap_or_default();
    let mut running = players(&ps);
    for r in &mut running {
        r.repo = repos.get(&r.session).cloned();
    }
    let kept = super::workspaces(&config.roots())
        .into_iter()
        .filter(|(id, _)| !running.iter().any(|r| &r.session == id))
        .map(|(id, dir)| Kept {
            repo: repos.get(&id).cloned(),
            session: id,
            dir: dir.to_string_lossy().into_owned(),
        })
        .collect();
    let log = match services.iter().find(|s| !s.old) {
        Some(s) => log_of(s).await,
        None => vec![],
    };
    Ok(Status {
        services,
        connection,
        slots: config.slots(),
        running,
        kept,
        log,
    })
}

/// The host's services here: launchd's agents in your session (and old daemons), or systemd's
/// user and system units.
async fn services(os: Os) -> Vec<Service> {
    let mut found = vec![];
    match os {
        Os::Macos => {
            let uid = service::uid().await.unwrap_or_default();
            for (label, by) in AGENTS {
                let target = format!("gui/{uid}/{label}");
                if let Ok(text) = service::output("launchctl", &["print", &target]).await {
                    let mut s = launchd(&text, "launchd agent", label);
                    s.by = Some(by.into());
                    found.push(s);
                }
            }
            for label in OLD_DAEMONS {
                let target = format!("system/{label}");
                if let Ok(text) = service::output("launchctl", &["print", &target]).await {
                    let mut s = launchd(&text, "launchd daemon", label);
                    s.old = true;
                    found.push(s);
                }
            }
        }
        Os::Linux => {
            const PROPERTIES: &str = "--property=LoadState,ActiveState,SubState,MainPID,\
                ExecMainStatus,FragmentPath,User,Environment";
            for (user, kind) in [(true, "systemd user unit"), (false, "systemd system unit")] {
                let mut args = vec!["show", UNIT, PROPERTIES];
                if user {
                    args.insert(0, "--user");
                }
                if let Ok(text) = service::output("systemctl", &args).await {
                    if let Some(s) = systemd(&text, kind) {
                        found.push(s);
                    }
                }
            }
        }
    }
    found
}

/// A launchd job, from `launchctl print`.
fn launchd(text: &str, kind: &str, label: &str) -> Service {
    let mut s = Service {
        kind: kind.into(),
        name: label.into(),
        ..Default::default()
    };
    let mut depth = 0usize;
    let mut block = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line == "}" {
            depth = depth.saturating_sub(1);
            continue;
        }
        if let Some(key) = line.strip_suffix(" = {") {
            depth += 1;
            if depth == 2 {
                block = key.to_string();
            }
            continue;
        }
        if depth == 2 && block == "environment" {
            if let Some((k, v)) = line.split_once(" => ") {
                s.env.insert(k.into(), v.into());
            }
            continue;
        }
        if depth != 1 {
            continue;
        }
        let Some((key, value)) = line.split_once(" = ") else {
            continue;
        };
        match key {
            "path" => s.file = Some(value.into()),
            "state" => s.state = value.into(),
            "pid" => s.pid = value.parse().ok(),
            "last exit code" => s.last_exit = Some(value.into()),
            "username" => s.user = Some(value.into()),
            "stdout path" => s.log = Some(value.into()),
            _ => {}
        }
    }
    s
}

/// A systemd unit, from `systemctl show`; none when it isn't there.
fn systemd(text: &str, kind: &str) -> Option<Service> {
    let props: HashMap<&str, &str> = text.lines().filter_map(|l| l.split_once('=')).collect();
    if props.get("LoadState").is_none_or(|s| *s != "loaded") {
        return None;
    }
    let state = match (props.get("ActiveState"), props.get("SubState")) {
        (Some(a), Some(s)) => format!("{a} ({s})"),
        (Some(a), None) => a.to_string(),
        _ => "unknown".into(),
    };
    let env = props
        .get("Environment")
        .map(|e| {
            e.split_whitespace()
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        })
        .unwrap_or_default();
    Some(Service {
        kind: kind.into(),
        name: UNIT.into(),
        by: None,
        file: props.get("FragmentPath").map(|p| p.to_string()),
        state,
        pid: props
            .get("MainPID")
            .and_then(|p| p.parse().ok())
            .filter(|p| *p != 0),
        last_exit: props.get("ExecMainStatus").map(|s| s.to_string()),
        user: props
            .get("User")
            .filter(|u| !u.is_empty())
            .map(|u| u.to_string()),
        log: None,
        old: false,
        env,
    })
}

/// The players in `ps`'s list (`pid user etime args`).
fn players(ps: &str) -> Vec<Running> {
    let mut found = vec![];
    for line in ps.lines() {
        let mut words = line.split_whitespace();
        let (Some(pid), Some(user), Some(elapsed)) = (words.next(), words.next(), words.next())
        else {
            continue;
        };
        let args: Vec<&str> = words.collect();
        let dirigent = args
            .first()
            .and_then(|a| Path::new(a).file_name())
            .is_some_and(|n| n.to_string_lossy().starts_with("dirigent"));
        if !dirigent || args.get(1) != Some(&crate::PLAYER) {
            continue;
        }
        let after = |flag: &str| {
            args.iter()
                .position(|a| *a == flag)
                .and_then(|i| args.get(i + 1))
                .map(|v| v.to_string())
        };
        let (Ok(pid), Some(session)) = (pid.parse(), after("--session")) else {
            continue;
        };
        found.push(Running {
            session,
            pid,
            user: user.into(),
            elapsed: elapsed.into(),
            workdir: after("--workdir"),
            repo: None,
        });
    }
    found
}

/// Whether control has the host `name` connected (as you see it), and your sessions'
/// repositories.
async fn from_control(http: reqwest::Client, name: &str) -> (Connection, HashMap<String, String>) {
    let mut connection = Connection {
        name: name.into(),
        connected: None,
        version: None,
        error: None,
    };
    let mut api = match Api::signed_in(http) {
        Ok(api) => api,
        Err(e) => {
            connection.error = Some(format!("{e:#}"));
            return (connection, HashMap::new());
        }
    };
    match api.get::<serde_json::Value>("/environments").await {
        Ok(env) => {
            let host = env["hosts"]
                .as_array()
                .and_then(|hs| hs.iter().find(|h| h["name"] == name));
            connection.connected = Some(host.is_some());
            connection.version = host.and_then(|h| h["version"].as_str().map(str::to_string));
        }
        Err(e) => connection.error = Some(format!("{e:#}")),
    }
    let repos = match api.get::<Vec<serde_json::Value>>("/sessions").await {
        Ok(sessions) => sessions
            .iter()
            .filter_map(|s| {
                let id = s["id"].as_str()?;
                let url = s["spec"]["source"]["git"]["url"].as_str()?;
                Some((id.to_string(), repo(url)))
            })
            .collect(),
        Err(_) => HashMap::new(),
    };
    (connection, repos)
}

/// A repository's URL as `host/owner/name`.
fn repo(url: &str) -> String {
    let url = url.trim_end_matches(".git");
    let url = url.split_once("://").map_or(url, |(_, rest)| rest);
    let url = url.rsplit_once('@').map_or(url, |(_, rest)| rest);
    url.replacen(':', "/", 1)
}

/// The last warnings and errors of `service`'s log: its file (launchd), or the journal.
async fn log_of(service: &Service) -> Vec<String> {
    let text = match &service.log {
        Some(path) => tail(Path::new(path)),
        None if service.kind.starts_with("systemd") => {
            let mut args = vec!["-u", UNIT, "-n", "500", "-o", "cat", "--no-pager"];
            if service.kind == "systemd user unit" {
                args.insert(0, "--user");
            }
            service::output("journalctl", &args)
                .await
                .unwrap_or_default()
        }
        None => String::new(),
    };
    problems(&text)
}

/// The end of the file at `path` (its last 64 KB).
fn tail(path: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map_or(0, |m| m.len());
    let _ = file.seek(SeekFrom::Start(len.saturating_sub(64 * 1024)));
    let mut bytes = vec![];
    let _ = file.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A log's last warnings and errors, without its colours.
fn problems(log: &str) -> Vec<String> {
    let lines: Vec<String> = log
        .lines()
        .map(plain)
        .filter(|l| l.contains(" WARN ") || l.contains(" ERROR ") || l.contains("panicked"))
        .collect();
    lines[lines.len().saturating_sub(LOG_LINES)..].to_vec()
}

/// `line` without ANSI escapes.
fn plain(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// `status` for reading.
pub fn render(s: &Status) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let home = std::env::var("HOME").unwrap_or_default();
    let short = |p: &str| match (!home.is_empty()).then(|| p.strip_prefix(&home)).flatten() {
        Some(rest) => format!("~{rest}"),
        None => p.to_string(),
    };
    let _ = writeln!(out, "Service");
    let current: Vec<&Service> = s.services.iter().filter(|x| !x.old).collect();
    if current.is_empty() {
        let _ = writeln!(
            out,
            "  none installed: `dirigent host install`, `brew services start dirigent`, or \
             home-manager's services.dirigent-host"
        );
    }
    for x in &s.services {
        let mut line = format!("  {} {}", x.kind, x.name);
        if let Some(by) = &x.by {
            let _ = write!(line, " ({by})");
        }
        let _ = write!(
            line,
            ": {}",
            if x.state.is_empty() {
                "unknown"
            } else {
                &x.state
            }
        );
        if let Some(pid) = x.pid {
            let _ = write!(line, ", pid {pid}");
        }
        if let Some(user) = &x.user {
            let _ = write!(line, ", as {user}");
        }
        if let Some(exit) = x
            .last_exit
            .as_deref()
            .filter(|e| x.pid.is_none() && *e != "0")
        {
            let _ = write!(line, ", last exit {exit}");
        }
        let _ = writeln!(out, "{line}");
        if x.old {
            let _ = writeln!(
                out,
                "    an earlier version's system daemon, outside your login session: remove it \
                 (nix-darwin: drop services.dirigent-host; else `sudo launchctl bootout \
                 system/{}` and delete its file)",
                x.name
            );
        }
        if let Some(file) = &x.file {
            let _ = writeln!(out, "    file {}", short(file));
        }
        if let Some(log) = &x.log {
            let _ = writeln!(out, "    log  {}", short(log));
        }
    }
    let c = &s.connection;
    let _ = writeln!(out, "Connection");
    let _ = match (c.connected, &c.error) {
        (Some(true), _) => writeln!(
            out,
            "  {}: connected{}",
            c.name,
            c.version
                .as_deref()
                .map_or(String::new(), |v| format!(", dirigent {v}"))
        ),
        (Some(false), _) => writeln!(out, "  {}: not connected", c.name),
        (None, Some(e)) => writeln!(out, "  {}: couldn't ask control: {e}", c.name),
        (None, None) => writeln!(out, "  {}: unknown", c.name),
    };
    let _ = writeln!(
        out,
        "Sessions running here: {} of {}",
        s.running.len(),
        s.slots
    );
    for r in &s.running {
        let _ = writeln!(
            out,
            "  {}  {}  pid {}, {}, as {}{}",
            r.session,
            r.repo.as_deref().unwrap_or("-"),
            r.pid,
            r.elapsed,
            r.user,
            r.workdir
                .as_deref()
                .map_or(String::new(), |w| format!("  {}", short(w)))
        );
    }
    if !s.kept.is_empty() {
        let _ = writeln!(out, "Workspaces kept (no player running)");
        for k in &s.kept {
            let _ = writeln!(
                out,
                "  {}  {}  {}",
                k.session,
                k.repo.as_deref().unwrap_or("-"),
                short(&k.dir)
            );
        }
    }
    if !s.log.is_empty() {
        let _ = writeln!(out, "Last warnings and errors in its log");
        for l in &s.log {
            let _ = writeln!(out, "  {l}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launchd_tells_the_agents_state_pid_exit_and_environment() {
        let text = "gui/501/dev.dirigent.host = {\n\
            \tactive count = 1\n\
            \tpath = /Users/adam/Library/LaunchAgents/dev.dirigent.host.plist\n\
            \tstate = running\n\
            \targuments = {\n\t\t/opt/homebrew/bin/dirigent\n\t\thost\n\t}\n\
            \tstdout path = /Users/adam/Library/Logs/dirigent-host.log\n\
            \tdefault environment = {\n\t\tPATH => /usr/bin:/bin\n\t}\n\
            \tenvironment = {\n\t\tDIRIGENT_HOST_CONFIG => /etc/dirigent/host.toml\n\t}\n\
            \tusername = adam\n\
            \tpid = 4312\n\
            \tlast exit code = 78: EX_CONFIG\n\
            \tresource coalition = {\n\t\tstate = active\n\t}\n\
            }\n";
        let s = launchd(text, "launchd agent", LABEL);
        assert_eq!(s.state, "running");
        assert_eq!(s.pid, Some(4312));
        assert_eq!(s.last_exit.as_deref(), Some("78: EX_CONFIG"));
        assert_eq!(s.user.as_deref(), Some("adam"));
        assert_eq!(
            s.file.as_deref(),
            Some("/Users/adam/Library/LaunchAgents/dev.dirigent.host.plist")
        );
        assert_eq!(
            s.log.as_deref(),
            Some("/Users/adam/Library/Logs/dirigent-host.log")
        );
        assert_eq!(
            s.env.get("DIRIGENT_HOST_CONFIG").map(String::as_str),
            Some("/etc/dirigent/host.toml")
        );
        // the default environment's isn't its own
        assert!(!s.env.contains_key("PATH"));
    }

    #[test]
    fn systemd_tells_a_units_state_or_that_it_isnt_there() {
        let text = "LoadState=loaded\nActiveState=active\nSubState=running\nMainPID=812\n\
            ExecMainStatus=0\nFragmentPath=/etc/systemd/system/dirigent-host.service\n\
            User=adam\nEnvironment=PATH=/run/current-system/sw/bin \
            DIRIGENT_HOST_CONFIG=/etc/dirigent/host.toml\n";
        let s = systemd(text, "systemd system unit").unwrap();
        assert_eq!(
            (s.state.as_str(), s.pid, s.user.as_deref()),
            ("active (running)", Some(812), Some("adam"))
        );
        assert_eq!(s.env["DIRIGENT_HOST_CONFIG"], "/etc/dirigent/host.toml");
        let stopped = systemd(
            "LoadState=loaded\nActiveState=failed\nSubState=failed\nMainPID=0\nUser=\n",
            "systemd user unit",
        )
        .unwrap();
        assert_eq!((stopped.pid, stopped.user), (None, None));
        assert_eq!(
            systemd(
                "LoadState=not-found\nActiveState=inactive\n",
                "systemd user unit"
            ),
            None
        );
    }

    #[test]
    fn the_players_are_found_in_the_process_list() {
        let ps = "  4312 adam     01:02:03 /opt/homebrew/bin/dirigent host run\n\
            \x20 5120 adam  1-02:00:01 /opt/homebrew/bin/dirigent __player --session 3f2a \
            --player-id p1 --workdir /Users/adam/dirigent/3f2a/workspace --state-dir x\n\
            \x20 5200 adam     00:05 vim __player --session nope\n";
        assert_eq!(
            players(ps),
            vec![Running {
                session: "3f2a".into(),
                pid: 5120,
                user: "adam".into(),
                elapsed: "1-02:00:01".into(),
                workdir: Some("/Users/adam/dirigent/3f2a/workspace".into()),
                repo: None,
            }]
        );
    }

    #[test]
    fn a_log_shows_its_last_warnings_and_errors_plain() {
        let log = "2026 INFO host starting\n\
            \x1b[33m2026  WARN\x1b[0m connecting to control: refused\n\
            2026 INFO connected\n\
            2026 ERROR a player failed\n";
        assert_eq!(
            problems(log),
            vec![
                "2026  WARN connecting to control: refused",
                "2026 ERROR a player failed"
            ]
        );
        assert_eq!(
            repo("https://github.com/adamchol/adamchol.git"),
            "github.com/adamchol/adamchol"
        );
        assert_eq!(repo("git@github.com:a/b.git"), "github.com/a/b");
    }

    #[test]
    fn status_reads_as_what_runs_where() {
        let s = Status {
            services: vec![
                Service {
                    kind: "launchd agent".into(),
                    name: LABEL.into(),
                    by: Some("dirigent host install".into()),
                    file: Some("/x/dev.dirigent.host.plist".into()),
                    state: "running".into(),
                    pid: Some(4312),
                    last_exit: Some("0".into()),
                    log: Some("/x/dirigent-host.log".into()),
                    ..Default::default()
                },
                Service {
                    kind: "launchd daemon".into(),
                    name: "org.nixos.dirigent-host".into(),
                    state: "spawn scheduled".into(),
                    last_exit: Some("78: EX_CONFIG".into()),
                    user: Some("adam".into()),
                    old: true,
                    ..Default::default()
                },
            ],
            connection: Connection {
                name: "macbook".into(),
                connected: Some(true),
                version: Some("0.1.12+abc".into()),
                error: None,
            },
            slots: 2,
            running: vec![Running {
                session: "3f2a".into(),
                pid: 5120,
                user: "adam".into(),
                elapsed: "01:12:03".into(),
                workdir: Some("/w/3f2a/workspace".into()),
                repo: Some("github.com/a/b".into()),
            }],
            kept: vec![Kept {
                session: "77de".into(),
                dir: "/w/77de".into(),
                repo: None,
            }],
            log: vec!["2026  WARN connecting to control: refused".into()],
        };
        let text = render(&s);
        for want in [
            "  launchd agent dev.dirigent.host (dirigent host install): running, pid 4312\n",
            "  launchd daemon org.nixos.dirigent-host: spawn scheduled, as adam, last exit 78: \
             EX_CONFIG\n",
            "    an earlier version's system daemon",
            "  macbook: connected, dirigent 0.1.12+abc\n",
            "Sessions running here: 1 of 2\n",
            "  3f2a  github.com/a/b  pid 5120, 01:12:03, as adam  /w/3f2a/workspace\n",
            "  77de  -  /w/77de\n",
            "  2026  WARN connecting to control: refused\n",
        ] {
            assert!(text.contains(want), "{want:?} in:\n{text}");
        }
    }
}
