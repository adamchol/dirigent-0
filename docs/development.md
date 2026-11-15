# Development

Working on dirigent itself: the dev stack, checks and tests, the test cluster, and releases.

## The dev shell and stack

```sh
nix develop      # Rust, Bun, Helm, the stack's services and scripts (dnvr)
dnvr up          # the whole stack, as local processes
```

The stack ([`nix/stack.nix`](../nix/stack.nix), [`nix/shell.nix`](../nix/shell.nix)) runs Redpanda,
ClickHouse (with Keeper), Kafka Connect with the exactly-once sink, the pause server, chlive,
Postgres, Garage, control and the web. Each picks its ports and publishes its address to the others;
migrations run on start. control runs in declared auth with the local runner: players are
processes under `.dnvr/sessions/`, on the mock model unless you start with `DIRIGENT_PLAYER_ARGS=`.

| Command | |
|---|---|
| `dnvr up` / `dnvr down` | Start / stop everything. |
| `dnvr ps`, `dnvr logs <process>` | Status and logs. |
| `dnvr restart <process>` | Restart one, e.g. `control` (rebuilds control and the player). |
| `dnvr session [--start] [--dirigent] [--git <url>] …` | Make a session through control, as `user:dev`. |
| `dnvr sessions`, `dnvr start <id>`, `dnvr stop [--now] <id>` | |
| `dnvr dirigent …` | The CLI, from source. |
| `dnvr ch`, `dnvr psql` | ClickHouse and Postgres clients. |
| `dnvr reset-data` | Delete the stack's data (while it's down). |
| `dnvr save-log <id> <name>` | Keep a session's log as one the player must go on loading (below). |

The web is at <http://127.0.0.1:54340>, control at `127.0.0.1:54330` (HTTP) and `:54331` (gRPC).

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test                      # unit tests (no services)
dnvr e2e                        # control's and the player's end-to-end tests, against the running stack
dnvr web-check                  # the web's types and tests
nixfmt --check $(git ls-files "*.nix")
nix flake check                 # the chart (version, rendering), migrations replayed, the players' VM booted (KVM)
```

The end-to-end tests are named `e2e_*` and run only with `DIRIGENT_E2E=1` (which `dnvr e2e` sets);
they use the mock model. Nothing in the tests calls a real model.

**Old logs.** `player/logs/<version>/` holds logs written by each release's player; every build runs
the player's `old_logs` test over them, so later players keep loading every earlier release's
sessions. Each release adds its own (`dnvr save-log`).

## The test cluster: kube-test

[`nix/kube-test/kube-test.sh`](../nix/kube-test/kube-test.sh) runs the chart on k3s with KubeVirt
and CloudNativePG, with stand-ins for ClickHouse and Kafka, on this machine or on a test instance
(EC2):

| Command | |
|---|---|
| `kube-test up` | k3s, KubeVirt, CloudNativePG, the stand-ins; load the images; install. |
| `kube-test install [-f values.yaml]` | Install or upgrade the chart. |
| `kube-test load` | Build and load the images again. |
| `kube-test e2e [--scenario default\|system]` | The cluster tests (`player/tests/kube.rs`): VMs finishing turns and handing over, machines, warm VMs, the cluster's own system and the shared store. Results go to `.dnvr/k3s/runs/<time>/`. |
| `kube-test remote <command>` | The same on the test instance: starts it, syncs the tree, runs, copies the results back. `kube-test remote stop` stops it. |
| `kube-test kubectl …`, `kube-test down`, `kube-test gc` | |

It needs `/dev/kvm` and a fair amount of memory (the test instance has 16 cores and 32 GB); the
instance is the EC2 machine tagged `project=dirigent-test`, started only for runs.

## Repository

| Path | |
|---|---|
| `player/` | The agent (`player/src/lib.rs`: its flags; `agent.rs`: the loop; `tools/`; `provider/`; `prompts/`) |
| `control/` | The control plane (`main.rs`: the API and leases; `runner.rs`, `pool.rs`, `hosts.rs`, `system.rs`, `nixstore.rs`, `prune.rs`, `auth.rs`) |
| `protocol/` | Events (`lib.rs`), `fold.rs`, rows and queries (`rows.rs`), control's types (`control.rs`) and protobuf |
| `sync/` | The workspace mirror |
| `cli/` | The CLI (`import/`, `host/`, `sync.rs`) |
| `web/` | The UI (React) and its Bun server |
| `chart/dirigent/` | The Helm chart |
| `db/` | Schemas and migrations (openmig), Postgres and ClickHouse |
| `nix/` | Packages (crane), images, the players' VM (`vm/`), the dev stack, kube-test, the Nix modules, the Homebrew formula |

## Releasing

A release is a tag `vX.Y.Z` on master:

1. Set `version` and `appVersion` in `chart/dirigent/Chart.yaml` to `X.Y.Z`.
2. Record the release's player logs: with the stack running the release's code, run a few mock
   sessions (turns with tool calls, an interrupted `?demo`, a stop and restart) and save each with
   `dnvr save-log <id> <name>` into `player/logs/X.Y.Z/`.
3. Turn the changelog's **Unreleased** into `## X.Y.Z (date)`.
4. Commit `chore: release X.Y.Z`, push master, tag `vX.Y.Z` and push the tag.

The tag's workflows build and push the images and the chart to `ghcr.io/adamchol` (on nixbuild.net;
the chart's version must match the tag), and build the CLI on macOS and update the Homebrew formula
in `adamchol/homebrew-dirigent`.
