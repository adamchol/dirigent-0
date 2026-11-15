//! Hosts: machines of users' that run players for their sessions (`dirigent host`), each
//! connected to control by a `Players.Host` call it holds, as its user. What's connected is all
//! control knows of them, kept here while it is: nothing of a host is stored. A session its
//! owner placed on a host is started there, through the host's [`Runner`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dirigent_protocol::control::pb;
use dirigent_protocol::control::Resources;
use serde::Serialize;
use tokio::sync::{mpsc, oneshot};
use tonic::Status;
use uuid::Uuid;

use crate::runner::Runner;

/// How long a host has to say it started a player.
const START_TIMEOUT: Duration = Duration::from_secs(30);

/// The hosts connected now, by their user and name.
#[derive(Default)]
pub struct Hosts {
    hosts: std::sync::Mutex<HashMap<(String, String), Arc<Host>>>,
    connections: AtomicU64,
}

/// A connected host.
pub struct Host {
    /// `host:<user>/<name>`, in the leases of the players it started.
    id: String,
    pub owner: String,
    pub hello: pb::Hello,
    /// This connection's, among the host's.
    conn: u64,
    tx: mpsc::Sender<Result<pb::ToHost, Status>>,
    /// Starts it's been asked for, by player, until it answers.
    starting: std::sync::Mutex<HashMap<String, oneshot::Sender<Result<(), String>>>>,
    /// The players it started, while it's connected.
    pub running: AtomicU64,
}

/// A host as the API lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Info {
    pub name: String,
    pub os: String,
    pub arch: String,
    pub cpus: u32,
    pub memory: u32,
    pub labels: Vec<String>,
    pub slots: u32,
    pub version: String,
    /// The players it started since it connected.
    pub started: u64,
}

impl Hosts {
    /// `owner`'s host saying `hello`, now connected, talking to control through `tx`: in place
    /// of one of the same name.
    pub fn connect(
        &self,
        owner: &str,
        hello: pb::Hello,
        tx: mpsc::Sender<Result<pb::ToHost, Status>>,
    ) -> Arc<Host> {
        let host = Arc::new(Host {
            id: format!("host:{owner}/{}", hello.name),
            owner: owner.into(),
            conn: self.connections.fetch_add(1, Ordering::Relaxed),
            hello,
            tx,
            starting: Default::default(),
            running: AtomicU64::new(0),
        });
        let key = (owner.to_string(), host.hello.name.clone());
        self.hosts.lock().unwrap().insert(key, host.clone());
        host
    }

    /// `host`'s connection ended: it's gone, unless another took its place.
    pub fn disconnect(&self, host: &Host) {
        let mut hosts = self.hosts.lock().unwrap();
        let key = (host.owner.clone(), host.hello.name.clone());
        if hosts.get(&key).is_some_and(|h| h.conn == host.conn) {
            hosts.remove(&key);
        }
    }

    /// `owner`'s host `name`, if it's connected.
    pub fn get(&self, owner: &str, name: &str) -> Option<Arc<Host>> {
        let key = (owner.to_string(), name.to_string());
        self.hosts.lock().unwrap().get(&key).cloned()
    }

    /// `owner`'s hosts, by name.
    pub fn of(&self, owner: &str) -> Vec<Info> {
        let hosts = self.hosts.lock().unwrap();
        let mut found: Vec<Info> = hosts
            .values()
            .filter(|h| h.owner == owner)
            .map(|h| h.info())
            .collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        found
    }

    /// `session` is gone: every host that has its workspace removes it (a host ignores the
    /// sessions it doesn't have).
    pub async fn forget(&self, session: Uuid) {
        let hosts: Vec<Arc<Host>> = self.hosts.lock().unwrap().values().cloned().collect();
        for host in hosts {
            let _ = host.forget(session).await;
        }
    }
}

impl Host {
    pub fn info(&self) -> Info {
        let h = &self.hello;
        Info {
            name: h.name.clone(),
            os: h.os.clone(),
            arch: h.arch.clone(),
            cpus: h.cpus,
            memory: h.memory,
            labels: h.labels.clone(),
            slots: h.slots,
            version: h.version.clone(),
            started: self.running.load(Ordering::Relaxed),
        }
    }

    /// The host's answer to starting `player`.
    pub fn answered(&self, player: &str, outcome: Result<(), String>) {
        if let Some(waiting) = self.starting.lock().unwrap().remove(player) {
            let _ = waiting.send(outcome);
        }
    }

    async fn send(&self, msg: pb::to_host::Msg) -> anyhow::Result<()> {
        self.tx
            .send(Ok(pb::ToHost { msg: Some(msg) }))
            .await
            .map_err(|_| anyhow::anyhow!("the host {} went", self.hello.name))
    }
}

#[async_trait::async_trait]
impl Runner for Host {
    fn id(&self) -> &str {
        &self.id
    }

    async fn start(
        &self,
        session: Uuid,
        player: &str,
        epoch: i64,
        token: &str,
        _system: Option<&str>,
        _machine: Resources,
    ) -> anyhow::Result<()> {
        let (answer, answered) = oneshot::channel();
        self.starting
            .lock()
            .unwrap()
            .insert(player.to_string(), answer);
        let start = pb::Start {
            session: session.to_string(),
            player: player.into(),
            epoch,
            token: token.into(),
        };
        self.send(pb::to_host::Msg::Start(start)).await?;
        let name = &self.hello.name;
        match tokio::time::timeout(START_TIMEOUT, answered).await {
            Ok(Ok(Ok(()))) => {
                self.running.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Ok(Ok(Err(why))) => anyhow::bail!("the host {name} couldn't start it: {why}"),
            Ok(Err(_)) => anyhow::bail!("the host {name} went"),
            Err(_) => {
                self.starting.lock().unwrap().remove(player);
                anyhow::bail!("the host {name} didn't start it in time")
            }
        }
    }

    async fn forget(&self, session: Uuid) -> anyhow::Result<()> {
        let forget = pb::Forget {
            session: session.to_string(),
        };
        self.send(pb::to_host::Msg::Forget(forget)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(name: &str) -> pb::Hello {
        pb::Hello {
            name: name.into(),
            os: "macos".into(),
            arch: "aarch64".into(),
            cpus: 10,
            memory: 32,
            slots: 2,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_host_is_its_users_and_the_last_to_connect_by_its_name() {
        let hosts = Hosts::default();
        let (tx, _rx) = mpsc::channel(4);
        let first = hosts.connect("user:a", hello("mac"), tx.clone());
        assert_eq!(first.id(), "host:user:a/mac");
        hosts.connect("user:b", hello("mac"), tx.clone());
        assert_eq!(hosts.of("user:a").len(), 1);
        assert!(hosts.get("user:a", "mac").is_some() && hosts.get("user:a", "pc").is_none());
        // another connection by the same name takes its place, and the first leaving doesn't
        // take the second with it
        let second = hosts.connect("user:a", hello("mac"), tx);
        hosts.disconnect(&first);
        assert!(hosts.get("user:a", "mac").is_some());
        hosts.disconnect(&second);
        assert!(hosts.get("user:a", "mac").is_none());
        assert_eq!(hosts.of("user:b")[0].name, "mac");
    }

    #[tokio::test]
    async fn a_start_is_what_the_host_answers() {
        let hosts = Hosts::default();
        let (tx, mut rx) = mpsc::channel(4);
        let host = hosts.connect("user:a", hello("mac"), tx);
        let s = Uuid::new_v4();
        let answering = {
            let host = host.clone();
            tokio::spawn(async move {
                let Some(Ok(pb::ToHost {
                    msg: Some(pb::to_host::Msg::Start(start)),
                })) = rx.recv().await
                else {
                    panic!("no Start");
                };
                assert_eq!((start.epoch, start.token.as_str()), (3, "t"));
                host.answered(&start.player, Err("full".into()));
                rx
            })
        };
        let machine = Resources {
            cpu: 1,
            memory: 1,
            disk: 1,
        };
        let failed = host.start(s, "p", 3, "t", None, machine).await;
        assert!(failed.unwrap_err().to_string().contains("full"));
        let mut rx = answering.await.unwrap();
        let answering = {
            let host = host.clone();
            tokio::spawn(async move {
                if let Some(Ok(pb::ToHost {
                    msg: Some(pb::to_host::Msg::Start(start)),
                })) = rx.recv().await
                {
                    host.answered(&start.player, Ok(()));
                }
            })
        };
        host.start(s, "q", 4, "t", None, machine).await.unwrap();
        answering.await.unwrap();
        assert_eq!(host.info().started, 1);
    }
}
