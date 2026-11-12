//! The players' machines: the cluster's default one (the VM template's, or `--machine-*` with
//! local players), the most a session may ask for (`--max-*`), and whether each user has
//! defaults of their own (`--defaults-per-user`, in their settings). A session's machine is
//! chosen when it's made and kept in its spec: none there, it's the default one as it is when
//! the session starts.

use serde::{Deserialize, Serialize};

use dirigent_protocol::control::Resources;

#[derive(clap::Args, Debug)]
pub struct MachineArgs {
    /// The default machine's cores, with local players (KubeVirt's is the VM template's).
    #[arg(long, default_value_t = 2)]
    pub machine_cpu: u32,
    /// Its memory, in GiB.
    #[arg(long, default_value_t = 4)]
    pub machine_memory: u32,
    /// Its workspace disk, in GiB.
    #[arg(long, default_value_t = 20)]
    pub machine_disk: u32,
    /// The most cores a session's machine may have; none, no limit.
    #[arg(long, env = "DIRIGENT_MAX_CPU")]
    pub max_cpu: Option<u32>,
    /// The most memory, in GiB.
    #[arg(long, env = "DIRIGENT_MAX_MEMORY")]
    pub max_memory: Option<u32>,
    /// The most workspace disk, in GiB.
    #[arg(long, env = "DIRIGENT_MAX_DISK")]
    pub max_disk: Option<u32>,
    /// Each user sets their own sessions' default machine (in their settings); otherwise it's
    /// the cluster's for everyone.
    #[arg(long, env = "DIRIGENT_DEFAULTS_PER_USER")]
    pub defaults_per_user: bool,
    /// How many VMs are kept booted for sessions to take, on the default machine; with
    /// `--defaults-per-user`, that many for each user active lately, on theirs.
    #[arg(long, env = "DIRIGENT_WARM", default_value_t = 0)]
    pub warm: usize,
    /// Who's active lately: a user who made a session within this many seconds.
    #[arg(long, env = "DIRIGENT_WARM_ACTIVE_WITHIN", default_value_t = 604800)]
    pub warm_active_within: u64,
}

/// A machine as given: what's left out is someone's default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Partial {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<u32>,
}

impl Partial {
    /// With what's left out of it from `under`.
    pub fn or(self, under: Partial) -> Partial {
        Partial {
            cpu: self.cpu.or(under.cpu),
            memory: self.memory.or(under.memory),
            disk: self.disk.or(under.disk),
        }
    }

    /// With what's left out from `defaults`.
    pub fn over(self, defaults: Resources) -> Resources {
        Resources {
            cpu: self.cpu.unwrap_or(defaults.cpu),
            memory: self.memory.unwrap_or(defaults.memory),
            disk: self.disk.unwrap_or(defaults.disk),
        }
    }
}

/// What the API tells of machines (`GET /machines`).
#[derive(Debug, Clone, Serialize)]
pub struct Machines {
    /// The cluster's default machine.
    pub defaults: Resources,
    /// The most a session's may have (none: no limit).
    pub limits: Partial,
    /// Whether users set their own defaults.
    pub per_user: bool,
    /// How many VMs are kept booted (per active user, with `per_user`).
    pub warm: usize,
    #[serde(skip)]
    pub active_within: std::time::Duration,
}

impl Machines {
    pub fn new(args: &MachineArgs, defaults: Resources) -> anyhow::Result<Machines> {
        let limits = Partial {
            cpu: args.max_cpu,
            memory: args.max_memory,
            disk: args.max_disk,
        };
        let machines = Machines {
            defaults,
            limits,
            per_user: args.defaults_per_user,
            warm: args.warm,
            active_within: std::time::Duration::from_secs(args.warm_active_within),
        };
        if let Some(why) = machines.over(defaults) {
            anyhow::bail!("the default machine is over the limits: {why}");
        }
        Ok(machines)
    }

    /// Why `r` is more than a session may have, if it is.
    pub fn over(&self, r: Resources) -> Option<String> {
        let over = |what: &str, have: u32, max: Option<u32>, unit: &str| {
            max.filter(|max| have > *max)
                .map(|max| format!("{have}{unit} {what}, at most {max}{unit}"))
        };
        let why: Vec<String> = [
            over("cores", r.cpu, self.limits.cpu, ""),
            over("of memory", r.memory, self.limits.memory, " GiB"),
            over("of disk", r.disk, self.limits.disk, " GiB"),
        ]
        .into_iter()
        .flatten()
        .collect();
        (!why.is_empty()).then(|| why.join(", "))
    }

    /// The machine a session that asked for `asked` (or none) gets, the defaults of `own`
    /// (its owner's, with per-user defaults) filling in: none when it asked for nothing and
    /// its owner has no defaults, which is the cluster's, whatever it is when it starts.
    pub fn resolve(&self, asked: Option<Partial>, own: Option<Partial>) -> Option<Resources> {
        let own = own.filter(|_| self.per_user);
        match (asked, own) {
            (None, None) => None,
            (asked, own) => Some(
                asked
                    .unwrap_or_default()
                    .or(own.unwrap_or_default())
                    .over(self.defaults),
            ),
        }
    }
}

/// A Kubernetes quantity (`4Gi`, `4096Mi`, `20G`…) in whole GiB; what isn't one is an error.
pub fn gib(quantity: &str) -> anyhow::Result<u32> {
    const GIB: u128 = 1 << 30;
    let q = quantity.trim();
    let split = q.find(|c: char| !c.is_ascii_digit()).unwrap_or(q.len());
    let (n, unit) = q.split_at(split);
    let n: u128 = n
        .parse()
        .map_err(|_| anyhow::anyhow!("{quantity:?} isn't a quantity"))?;
    let scale: u128 = match unit {
        "Ki" => 1 << 10,
        "Mi" => 1 << 20,
        "Gi" => 1 << 30,
        "Ti" => 1 << 40,
        "k" => 1_000,
        "M" => 1_000_000,
        "G" => 1_000_000_000,
        "T" => 1_000_000_000_000,
        "" => 1,
        _ => anyhow::bail!("{quantity:?}: no unit {unit:?}"),
    };
    let bytes = n * scale;
    anyhow::ensure!(
        bytes.is_multiple_of(GIB) && bytes > 0,
        "{quantity:?} isn't a whole number of GiB"
    );
    u32::try_from(bytes / GIB).map_err(|_| anyhow::anyhow!("{quantity:?} is too much"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(cpu: u32, memory: u32, disk: u32) -> Resources {
        Resources { cpu, memory, disk }
    }

    fn machines(per_user: bool, limits: Partial) -> Machines {
        Machines {
            defaults: r(2, 4, 20),
            limits,
            per_user,
            warm: 0,
            active_within: Default::default(),
        }
    }

    #[test]
    fn quantities_are_whole_gib() {
        assert_eq!(gib("4Gi").unwrap(), 4);
        assert_eq!(gib("4096Mi").unwrap(), 4);
        assert_eq!(gib("1Ti").unwrap(), 1024);
        assert!(gib("1536Mi").is_err());
        assert!(gib("4G").is_err());
        assert!(gib("four").is_err());
        assert!(gib("4Xi").is_err());
        assert!(gib("0Gi").is_err());
    }

    #[test]
    fn a_session_gets_what_it_asked_for_then_its_owners_defaults_then_the_clusters() {
        let m = machines(true, Partial::default());
        let cpu = |n| Partial {
            cpu: Some(n),
            ..Default::default()
        };
        let memory = |n| Partial {
            memory: Some(n),
            ..Default::default()
        };
        assert_eq!(m.resolve(None, None), None);
        assert_eq!(m.resolve(Some(cpu(8)), None), Some(r(8, 4, 20)));
        assert_eq!(m.resolve(None, Some(memory(16))), Some(r(2, 16, 20)));
        assert_eq!(
            m.resolve(Some(cpu(8)), Some(memory(16))),
            Some(r(8, 16, 20))
        );
        // the owner's defaults count only where users have their own
        let m = machines(false, Partial::default());
        assert_eq!(m.resolve(None, Some(memory(16))), None);
        assert_eq!(m.resolve(Some(cpu(8)), Some(memory(16))), Some(r(8, 4, 20)));
    }

    #[test]
    fn what_is_over_the_limits_says_so() {
        let m = machines(
            false,
            Partial {
                cpu: Some(8),
                memory: None,
                disk: Some(100),
            },
        );
        assert_eq!(m.over(r(8, 512, 100)), None);
        let why = m.over(r(9, 4, 200)).unwrap();
        assert!(why.contains("9 cores, at most 8"), "{why}");
        assert!(why.contains("200 GiB of disk, at most 100 GiB"), "{why}");
    }
}
