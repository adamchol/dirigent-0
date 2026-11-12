//! Warm VMs (`--warm`): players' VMs booted before any session asks for one, so that a starting
//! session takes one instead of waiting for a VM to boot. Each runs a warm player, which gets
//! ready as far as it can without a session and waits (`Players.Wait`) to be given one. There are
//! `--warm` on the default machine; with per-user defaults, that many for each user who made a
//! session lately, on their default machine. A session takes one on its own machine, if one's
//! waiting; one on another machine waits for a VM of its own, as do all without a waiting one.
//!
//! A warm VM is current while its key is: what it was made from (the runner's VM template, its
//! machine, the cluster's players' system and the shared Nix store's generation, so a new one of
//! any is a new key); one that isn't is replaced. The pool is kept by rounds, every 30 s and
//! whenever what it should be may have changed ([`Pool::wake`]: a take, settings saved, a new
//! system or generation): each deletes the VMs not current, those over the count, and those that
//! never came, and makes those missing. What's warm is the runner's VMs (their labels), and which
//! of their players wait, here: nothing of it is kept.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;

use dirigent_protocol::control::{pb, Resources};
use tokio::sync::{mpsc, Notify};
use tonic::Status;
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::runner::Warm;
use crate::Control;

/// The warm players waiting to be given a session, and what wakes the pool's round.
#[derive(Default)]
pub struct Pool {
    waiting: Mutex<HashMap<String, Waiting>>,
    wake: Notify,
}

/// A warm player waiting, on its VM's machine and key: told its session on `tx`.
struct Waiting {
    machine: Resources,
    key: String,
    tx: mpsc::Sender<Result<pb::Assigned, Status>>,
}

/// A warm player taken for a session, to be told it (or given back).
pub struct Taken {
    pub player: String,
    waiting: Waiting,
}

impl Taken {
    /// Tell it its session, `session`'s `epoch`th lease: whether it's still there to be told.
    pub async fn tell(self, session: Uuid, epoch: i64) -> bool {
        let assigned = pb::Assigned {
            session: session.to_string(),
            epoch,
        };
        self.waiting.tx.send(Ok(assigned)).await.is_ok()
    }
}

impl Pool {
    /// Its round is due: what the pool should be may have changed.
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    /// A waiting player on `machine`, its VM current (`key`), taken.
    pub fn take(&self, machine: Resources, key: &str) -> Option<Taken> {
        let mut waiting = self.waiting.lock().unwrap();
        waiting.retain(|_, w| !w.tx.is_closed());
        let player = waiting
            .iter()
            .find(|(_, w)| w.machine == machine && w.key == key)
            .map(|(p, _)| p.clone())?;
        let w = waiting.remove(&player).expect("found");
        Some(Taken { player, waiting: w })
    }

    /// A player taken for no session after all: it waits still.
    pub fn give_back(&self, taken: Taken) {
        let mut waiting = self.waiting.lock().unwrap();
        waiting.insert(taken.player, taken.waiting);
    }

    fn waiting(&self) -> Vec<String> {
        let mut waiting = self.waiting.lock().unwrap();
        waiting.retain(|_, w| !w.tx.is_closed());
        waiting.keys().cloned().collect()
    }

    fn forget(&self, player: &str) {
        self.waiting.lock().unwrap().remove(player);
    }
}

/// The system a warm VM is made in: the cluster's own, once it's ready, where the runner runs it.
fn system(control: &Control) -> Option<String> {
    let runner = control.runner?;
    control
        .system
        .as_ref()
        .filter(|_| runner.runs_systems())
        .and_then(|s| s.ready())
}

/// The key of a warm VM on `machine` made now.
pub fn key(control: &Control, machine: Resources) -> Option<String> {
    let runner = control.runner.filter(|r| r.warms())?;
    Some(runner.warm_key(machine, system(control).as_deref()))
}

/// The warm `player` waits (`Players.Wait`), its VM's machine running `running`: the stream it's
/// told its session on. NOT_FOUND unless its VM is a warm one that's current, in the system it
/// was made in.
pub async fn wait(
    control: &'static Control,
    player: String,
    running: Option<String>,
) -> Result<mpsc::Receiver<Result<pb::Assigned, Status>>, Status> {
    let Some(runner) = control.runner.filter(|r| r.warms()) else {
        return Err(Status::not_found("control keeps no warm VMs"));
    };
    let vms = runner.warm_vms().await.map_err(|e| {
        error!("listing the warm VMs: {e:#}");
        Status::unavailable("the warm VMs can't be listed")
    })?;
    let Some(vm) = vms.into_iter().find(|v| v.player == player) else {
        return Err(Status::not_found("no warm VM of this player's"));
    };
    let system = system(control);
    if vm.key != runner.warm_key(vm.machine, system.as_deref()) {
        return Err(Status::not_found("its VM isn't current"));
    }
    if system.is_some() && running != system {
        warn!(
            player,
            running, system, "a warm VM didn't come up with the players' system"
        );
        tokio::spawn(async move { runner.unwarm(&player).await });
        return Err(Status::not_found("its VM runs another system"));
    }
    let (tx, rx) = mpsc::channel(1);
    let waiting = Waiting {
        machine: vm.machine,
        key: vm.key,
        tx,
    };
    info!(player, machine = ?vm.machine, "a warm player waits");
    control.pool.waiting.lock().unwrap().insert(player, waiting);
    Ok(rx)
}

/// Keep the pool: a round every 30 s, and when woken. Without warm VMs, one round, which
/// deletes those kept before.
pub async fn keep(control: &'static Control) {
    if !control.runner.is_some_and(|r| r.warms()) {
        if control.machines.warm > 0 {
            warn!("--warm: this runner keeps no warm VMs");
        }
        return;
    }
    loop {
        if let Err(e) = round(control).await {
            error!("keeping the warm VMs: {e:#}");
        }
        if control.machines.warm == 0 {
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            _ = control.pool.wake.notified() => {}
        }
    }
}

async fn round(control: &'static Control) -> anyhow::Result<()> {
    let runner = control.runner.expect("warms");
    let want = want(control).await?;
    let vms = runner.warm_vms().await?;
    let system = system(control);
    let key = |m: Resources| runner.warm_key(m, system.as_deref());
    let plan = plan(&want, &vms, key, &control.pool.waiting(), control.startup);
    for (player, why) in &plan.delete {
        info!(player, why, "deleting a warm VM");
        control.pool.forget(player);
        runner.unwarm(player).await;
    }
    for machine in plan.make {
        let player = Uuid::new_v4().to_string();
        runner
            .warm(&player, machine, system.as_deref(), &key(machine))
            .await?;
        info!(player, ?machine, system, "made a warm VM");
    }
    Ok(())
}

/// How many warm VMs there should be, by machine.
async fn want(control: &Control) -> anyhow::Result<HashMap<Resources, usize>> {
    let machines = &control.machines;
    let mut want = HashMap::new();
    if !machines.per_user {
        want.insert(machines.defaults, machines.warm);
        return Ok(want);
    }
    for settings in control.store.active_owners(machines.active_within).await? {
        let own = settings.and_then(|s| crate::settings::Settings::kept(s).machine);
        let machine = own.unwrap_or_default().over(machines.defaults);
        *want.entry(machine).or_default() += machines.warm;
    }
    Ok(want)
}

/// What a round does.
#[derive(Debug, Default, PartialEq, Eq)]
struct Plan {
    /// The players whose VMs go, and why.
    delete: Vec<(String, &'static str)>,
    /// The machines of the VMs to make.
    make: Vec<Resources>,
}

/// From `vms` (with the players `waiting`), to `want`, VMs being current by `key`: those not
/// current go, as do those older than `startup` whose player never waited, and those over the
/// count (those not waiting first, then the newest); and those missing are made.
fn plan(
    want: &HashMap<Resources, usize>,
    vms: &[Warm],
    key: impl Fn(Resources) -> String,
    waiting: &[String],
    startup: Duration,
) -> Plan {
    let mut plan = Plan::default();
    let mut kept: HashMap<Resources, Vec<&Warm>> = HashMap::new();
    for vm in vms {
        let waits = waiting.contains(&vm.player);
        if vm.key != key(vm.machine) {
            plan.delete.push((vm.player.clone(), "not current"));
        } else if !waits && vm.age > startup {
            plan.delete
                .push((vm.player.clone(), "its player never came"));
        } else {
            kept.entry(vm.machine).or_default().push(vm);
        }
    }
    let machines: HashSet<Resources> = want.keys().chain(kept.keys()).copied().collect();
    for machine in machines {
        let mut vms = kept.remove(&machine).unwrap_or_default();
        let n = want.get(&machine).copied().unwrap_or(0);
        // the ones to keep first: those waiting, then the oldest
        vms.sort_by_key(|v| (!waiting.contains(&v.player), std::cmp::Reverse(v.age)));
        for vm in vms.iter().skip(n) {
            plan.delete.push((vm.player.clone(), "more than wanted"));
        }
        plan.make
            .extend(std::iter::repeat_n(machine, n.saturating_sub(vms.len())));
    }
    plan.make.sort_by_key(|m| (m.cpu, m.memory, m.disk));
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL: Resources = Resources {
        cpu: 2,
        memory: 4,
        disk: 20,
    };
    const BIG: Resources = Resources {
        cpu: 8,
        memory: 32,
        disk: 100,
    };

    fn vm(player: &str, machine: Resources, key: &str, age: u64) -> Warm {
        Warm {
            player: player.into(),
            machine,
            key: key.into(),
            age: Duration::from_secs(age),
        }
    }

    fn key(m: Resources) -> String {
        format!("k{}", m.cpu)
    }

    const STARTUP: Duration = Duration::from_secs(600);

    #[test]
    fn the_wanted_vms_are_made_and_kept() {
        let want = HashMap::from([(SMALL, 2), (BIG, 1)]);
        let p = plan(&want, &[], key, &[], STARTUP);
        assert_eq!(p.delete, vec![]);
        assert_eq!(p.make, vec![SMALL, SMALL, BIG]);
        let vms = [
            vm("a", SMALL, "k2", 5),
            vm("b", SMALL, "k2", 700),
            vm("c", BIG, "k8", 1),
        ];
        let p = plan(&want, &vms, key, &["b".into()], STARTUP);
        assert_eq!(p, Plan::default());
    }

    #[test]
    fn those_not_current_never_come_or_over_the_count_go() {
        let want = HashMap::from([(SMALL, 1)]);
        let vms = [
            vm("old", SMALL, "k-before", 5),
            vm("lost", SMALL, "k2", 700),
            vm("waits", SMALL, "k2", 30),
            vm("boots", SMALL, "k2", 10),
            vm("big", BIG, "k8", 10),
        ];
        let p = plan(&want, &vms, key, &["waits".into()], STARTUP);
        let mut deleted: Vec<_> = p.delete.iter().map(|(p, _)| p.as_str()).collect();
        deleted.sort();
        assert_eq!(deleted, vec!["big", "boots", "lost", "old"]);
        assert_eq!(p.make, vec![]);
        // none waiting: the oldest is kept, the stale one made again
        let vms = [
            vm("old", SMALL, "k-before", 5),
            vm("b1", SMALL, "k2", 30),
            vm("b2", SMALL, "k2", 10),
        ];
        let p = plan(&HashMap::from([(SMALL, 2)]), &vms, key, &[], STARTUP);
        assert_eq!(p.delete, vec![("old".into(), "not current")]);
        assert_eq!(p.make, vec![]);
        let p = plan(&HashMap::from([(SMALL, 3)]), &vms, key, &[], STARTUP);
        assert_eq!(p.make, vec![SMALL]);
        // all of a machine's stale: as many made again
        let p = plan(
            &HashMap::from([(BIG, 1)]),
            &[vm("old", BIG, "k-before", 5)],
            key,
            &[],
            STARTUP,
        );
        assert_eq!(p.make, vec![BIG]);
    }
}
