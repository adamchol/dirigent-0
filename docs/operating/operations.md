# Operations

Upgrading, watching, backing up and troubleshooting an install.

## Upgrading

```sh
helm upgrade dirigent oci://ghcr.io/adamchol/charts/dirigent --version <new> \
  -n dirigent -f my-values.yaml --timeout 20m --wait --wait-for-jobs
```

Read the [changelog](../../CHANGELOG.md) first: it says what each release changes for operators.

- **Migrations** run by themselves: control's `migrate` init container applies Postgres' and
  ClickHouse's before control starts, on every start (a no-op when there's nothing new).
- **control** is replaced (one replica, `Recreate`): for a few seconds no session can start, and
  players lose their connection. They keep their sessions for a short grace and reconnect; running
  turns go on.
- **Warm VMs** are replaced. **Running sessions' VMs** keep their old player until the session
  stops; the next start gets the new one.
- **The players' system** (`player.nixos`) is rebuilt only if the VM image changed.
- **The ingestion's** hooks re-run: the topic's settings are re-applied and the connector
  re-registered.

The chart's version and its images' always match: `images.tag` empty means the chart's
appVersion.

## Status and logs

| What | Where |
|---|---|
| control | `kubectl -n dirigent logs deploy/dirigent-control` (`RUST_LOG` through `control.env`). Its init containers: `-c secrets`, `-c migrate`. |
| chlive | `kubectl -n dirigent logs deploy/dirigent-chlive` |
| The web | `kubectl -n dirigent logs deploy/dirigent-web` |
| A player | Its VM's serial console, which has the guest's journal: `kubectl -n dirigent logs <virt-launcher pod> -c guest-console-log`. More detail: `player.env: {RUST_LOG: dirigent_player=debug}`. |
| Players' VMs | `kubectl -n dirigent get vmi` (`player-…`, `warm-…`, `build-…`) |
| The players' system | `kubectl -n dirigent describe configmap dirigent-player-nixos` (Events), or `GET /api/system` |
| Versions and health | The web's account menu → **About and debug info**, `dirigent version`, or `GET /api/about`: every part's version, and the install's pods with their images, readiness and restarts. |
| The ingestion | `kubectl -n dirigent logs deploy/dirigent-ingest-connect`; the connector's state: `kubectl -n dirigent exec deploy/dirigent-ingest-connect -- curl -s localhost:8083/connectors/dirigent-ingest-session-events/status` (its task should be `RUNNING`) |
| A session's log | In ClickHouse: `SELECT ts, kind, author, data FROM dirigent.session_events WHERE session_id = '<id>' ORDER BY offset` |

control deletes VMs that powered off or failed, and VMs whose player never took its lease within
`control.startup`, every 30 seconds.

## Data and retention

| What | Where | Kept |
|---|---|---|
| Sessions' events | Kafka topic `session_events`, then ClickHouse `dirigent.session_events` | Kafka: 90 days. ClickHouse: until the session is deleted; streamed deltas (`model_delta`) one day. |
| Sessions, access, users' keys, secrets, settings | Postgres (`dirigent-postgres`) | Until deleted. Keys and secrets are encrypted with the link key. |
| Workspace checkpoints | kopia repository in S3 (`checkpoints.s3.bucket`) | Until the session is deleted. |
| The players' Nix cache | S3 (`nix.cache.bucket`) | Built systems; not pruned. |
| The shared Nix store | Volume `dirigent-nix-store` | `nix.shared.keep` generations. |

### Deleting a session

Deleting an (archived) session removes, in the background: its VMs, its directory on a host, its
checkpoints, then (a minute later) its events in ClickHouse, then its row in Postgres (with its
secrets). Messages stay in the Kafka topic until its retention runs out, and in chlive's snapshot
tables until they're replaced.

### Secrets to keep

control makes these Secrets at its first start and never replaces a value. They have no owner, so
they survive `helm uninstall`. **Back them up**: losing some makes data unreadable.

| Secret | Holds | Lost, you lose |
|---|---|---|
| `dirigent-link-key` | The key that encrypts users' API keys, ChatGPT links and secrets | All of them (users must add them again) |
| `dirigent-kopia-password` | The kopia repository's password | Every checkpoint |
| `dirigent-signing-key` | control's token signing key | Nothing lasting: sign-ins and tokens are reissued |
| `dirigent-nix-signing` | The Nix cache's signing key | The cache's trust (systems are rebuilt) |
| `dirigent-control-postgres`, `dirigent-chlive-postgres` | Postgres roles' passwords | Recreated with new passwords |
| `dirigent-clickhouse`, `dirigent-clickhouse-sink`, `-pause`, `-recovery` | ClickHouse users' passwords | Recreated on the next start |
| `dirigent-garage` | Garage's key (with `checkpoints.garage.enabled`) | Access to Garage's data |
| `dirigent-kopia` | The players' kopia token (made by a hook Job) | Remade by the next upgrade |

### Backups

The chart doesn't back up its Postgres (configure CloudNativePG's backups on the `dirigent-postgres`
Cluster if you need them), and ClickHouse and Kafka are yours. What's worth keeping, in order:

1. The Secrets above.
2. Postgres: sessions, who may use them, users' keys and secrets.
3. ClickHouse's `dirigent.session_events`: every session's log. (Kafka has its last 90 days.)
4. The S3 buckets: checkpoints (workspaces can mostly be rebuilt from the log; ignored files and
   files over 50 MB can't).

## Troubleshooting

**A session stays "Starting a machine…".** Its VM is booting, or pulling the image (first start on
a node). `kubectl get vmi` and the launcher pod's events show which. A player that doesn't take its
lease within `control.startup` (600 s) is given up and another is started.

**A session's turn fails at once.** The chat shows why: usually no Anthropic key ("no Anthropic API
key: …"), a refused key (`API error 401`), a broken ChatGPT link, or a plan's usage limit.

**Sessions show nothing new; the web stays on old events.** The ingestion may be stuck. Check the
connector's state (above) and Kafka Connect's log. After Kafka, Keeper or node restarts, Kafka
Connect, the pause server and chlive may need a restart:

```sh
kubectl -n dirigent rollout restart deploy/dirigent-ingest-connect deploy/dirigent-ingest-pause-server
kubectl -n dirigent rollout restart deploy/dirigent-chlive
```

A restarted Kafka Connect hands its connector back to a worker only after its rebalance delay (about
five minutes); until then, chlive can't serve logs.

**Every event is dropped, on ClickHouse before 26.x.** dirigent 0.1.8 stored message times in a form
those versions refuse by default. Upgrade to 0.1.9 or later (it sets
`date_time_input_format=best_effort`); events lost meanwhile can be ingested again from the topic
while it has them.

**Players are refused: "control leases sessions only to the players it started".** A player without
a valid lease token tried to take a session: an older release's player (0.1.9 changed leases), or a
VM booted into a players' system built with an older player. Upgrading replaces them as sessions
restart.

**A session's command hangs, and the VM won't stop.** With the shared Nix store, the VM's
virtiofsd (its pod's `virtiofs-dirigent-nix` container) may have run out of memory:
`kubectl -n dirigent get pod <virt-launcher pod>` shows `OOMKilled`, and the VM's console
`task … blocked for more than … seconds`. Whatever reads the store in that VM then hangs for good,
and can't be killed. A stop still ends the turn (the command is left behind), but the VM's own
shutdown hangs too, until `player.stopGrace` is up. Delete its launcher pod with
`--grace-period=0 --force`, and give virtiofsd more (`nix.shared.virtiofs.resources`): new VMs
get it.

**The players' system is broken.** See [Machines and
systems](machines-and-systems.md#the-clusters-own-system): the Events say why, and the stock system
runs until the configuration changes.

**A host doesn't appear.** See [Hosts](../guides/hosts.md#requirements): the host must reach
control's players' service (`hosts.expose`, or a private `players` address) and be signed in as the
user looking for it.

## Uninstalling

```sh
helm uninstall dirigent -n dirigent
```

This leaves the Secrets control made, `dirigent-kopia`, the shared store's volume, Garage's volume,
and everything in your ClickHouse, Kafka and S3. The ingestion's connector is unregistered.
