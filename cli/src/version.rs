//! `dirigent version`: this CLI's version, and what the dirigent it's signed in to runs
//! (control's `GET /about`, and your connected hosts from `GET /environments`).

use std::fmt::Write;

use serde::{Deserialize, Serialize};

use crate::api::Api;

/// `GET /about`.
#[derive(Deserialize, Serialize)]
pub struct About {
    pub control: Control,
    pub players: Players,
    pub postgres: Option<String>,
    pub clickhouse: Option<String>,
    pub pods: Option<Vec<Pod>>,
    #[serde(default)]
    pub errors: Vec<String>,
}

#[derive(Deserialize, Serialize)]
pub struct Control {
    pub version: String,
    pub started: String,
    pub runner: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub struct Players {
    pub vm: String,
    pub system: Option<System>,
}

#[derive(Deserialize, Serialize)]
pub struct System {
    pub configured: bool,
    pub state: Option<String>,
    pub store_path: Option<String>,
    pub error: Option<String>,
}

#[derive(Deserialize, Serialize)]
pub struct Pod {
    pub name: String,
    pub component: String,
    pub images: Vec<String>,
    pub phase: String,
    pub ready: bool,
    pub restarts: i32,
    pub started: Option<String>,
}

/// One of your connected hosts, as `GET /environments` tells it.
#[derive(Deserialize, Serialize)]
pub struct Host {
    pub name: String,
    pub os: String,
    pub arch: String,
    pub version: String,
    pub started: u32,
    pub slots: u32,
}

#[derive(Deserialize)]
struct Environments {
    hosts: Vec<Host>,
}

/// What the dirigent runs, and your hosts.
pub async fn fetch(api: &mut Api) -> anyhow::Result<(About, Vec<Host>)> {
    let about: About = api.get("/about").await?;
    let environments: Environments = api.get("/environments").await?;
    Ok((about, environments.hosts))
}

/// `about` and `hosts` as `dirigent version` prints them, under this CLI's version `cli`, for a
/// dirigent at `url`.
pub fn render(cli: &str, url: &str, about: &About, hosts: &[Host]) -> String {
    let mut out = String::new();
    row(&mut out, "cli", cli);
    row(&mut out, "dirigent", url);
    let c = &about.control;
    let runner = c.runner.as_deref().unwrap_or("none");
    row(
        &mut out,
        "control",
        &format!("{}, started {}, runner {runner}", c.version, c.started),
    );
    let system = match &about.players.system {
        None => "unknown".to_string(),
        Some(System {
            configured: false, ..
        }) => "the image's own".to_string(),
        Some(s) => {
            let mut t = s.state.clone().unwrap_or_else(|| "not built".into());
            if let Some(p) = &s.store_path {
                let _ = write!(t, " {p}");
            }
            if let Some(e) = &s.error {
                let _ = write!(t, ": {e}");
            }
            t
        }
    };
    row(
        &mut out,
        "players",
        &format!("VM {}, system {system}", about.players.vm),
    );
    row(
        &mut out,
        "postgres",
        about.postgres.as_deref().unwrap_or("unknown"),
    );
    row(
        &mut out,
        "clickhouse",
        about.clickhouse.as_deref().unwrap_or("unknown"),
    );
    match &about.pods {
        None => row(&mut out, "pods", "none (not in Kubernetes)"),
        Some(pods) => {
            row(&mut out, "pods", "");
            // columns as wide as their widest
            let wc = pods.iter().map(|p| p.component.len()).max().unwrap_or(0);
            let wn = pods.iter().map(|p| p.name.len()).max().unwrap_or(0);
            for p in pods {
                let health = if p.ready {
                    "ready".to_string()
                } else {
                    p.phase.to_lowercase()
                };
                let restarts = match p.restarts {
                    0 => String::new(),
                    1 => ", 1 restart".into(),
                    n => format!(", {n} restarts"),
                };
                let _ = writeln!(
                    out,
                    "  {:<wc$}  {:<wn$}  {:<18}  {}",
                    p.component,
                    p.name,
                    format!("{health}{restarts}"),
                    p.images.join(" ")
                );
            }
        }
    }
    if hosts.is_empty() {
        row(&mut out, "your hosts", "none connected");
    } else {
        row(&mut out, "your hosts", "");
        for h in hosts {
            let _ = writeln!(
                out,
                "  {:<14} {}, {} {}, {}/{} sessions",
                h.name, h.version, h.os, h.arch, h.started, h.slots
            );
        }
    }
    for e in &about.errors {
        let _ = writeln!(out, "couldn't read {e}");
    }
    out
}

/// A line: what, and its text after, aligned (none: a heading for the lines under it).
fn row(out: &mut String, what: &str, text: &str) {
    let _ = match text {
        "" => writeln!(out, "{what}"),
        _ => writeln!(out, "{what:<11} {text}"),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_tells_the_cli_control_players_databases_pods_and_hosts() {
        let about: About = serde_json::from_value(serde_json::json!({
            "control": {
                "version": "0.1.12+abc1234",
                "started": "2026-11-15T12:00:00Z",
                "runner": "kube:dirigent/dirigent",
            },
            "players": {
                "vm": "0.1.12+abc1234",
                "system": { "configured": true, "state": "ready", "store_path": "/nix/store/x" },
            },
            "postgres": "17.2",
            "clickhouse": null,
            "pods": [
                { "name": "dirigent-chlive-0", "component": "chlive", "images": ["chlive:sha-1"],
                  "phase": "Running", "ready": true, "restarts": 0 },
                { "name": "dirigent-web-1", "component": "web", "images": ["web:0.1.12"],
                  "phase": "Pending", "ready": false, "restarts": 2 },
            ],
            "errors": ["ClickHouse: no answer in 5 seconds"],
        }))
        .unwrap();
        let hosts = [Host {
            name: "macbook".into(),
            os: "macOS 15.2".into(),
            arch: "aarch64".into(),
            version: "0.1.12+abc1234".into(),
            started: 1,
            slots: 2,
        }];
        assert_eq!(
            render("0.1.12+abc1234", "https://d.example.com", &about, &hosts),
            "\
cli         0.1.12+abc1234
dirigent    https://d.example.com
control     0.1.12+abc1234, started 2026-11-15T12:00:00Z, runner kube:dirigent/dirigent
players     VM 0.1.12+abc1234, system ready /nix/store/x
postgres    17.2
clickhouse  unknown
pods
  chlive  dirigent-chlive-0  ready               chlive:sha-1
  web     dirigent-web-1     pending, 2 restarts  web:0.1.12
your hosts
  macbook        0.1.12+abc1234, macOS 15.2 aarch64, 1/2 sessions
couldn't read ClickHouse: no answer in 5 seconds
"
        );
    }

    #[test]
    fn outside_kubernetes_without_hosts_or_a_system_of_its_own() {
        let about: About = serde_json::from_value(serde_json::json!({
            "control": { "version": "dev", "started": "2026-11-15T12:00:00Z", "runner": null },
            "players": { "vm": "0.1.0", "system": { "configured": false } },
            "postgres": "17.2",
            "clickhouse": "26.7.2.59",
            "pods": null,
        }))
        .unwrap();
        let out = render("dev", "http://localhost:3000", &about, &[]);
        assert!(out.contains("runner none\n"), "{out}");
        assert!(out.contains("system the image's own\n"), "{out}");
        assert!(
            out.contains("pods        none (not in Kubernetes)\n"),
            "{out}"
        );
        assert!(out.contains("your hosts  none connected\n"), "{out}");
    }
}
