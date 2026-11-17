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

It needs `/dev/kvm` and a fair amount of memory. A pull request's `e2e` workflow runs `up`, `install`
and `e2e` on a GitHub runner (4 cores, 16 GB), the images built on nixbuild.net; `remote` runs them on
the EC2 machine tagged `project=dirigent-test` (16 cores, 32 GB), started only for runs.

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

**Images and caches.** The images (`nix/images.nix`) are built with nix2container: each layer is a
derivation of its own, and what changes least has one (Bun, kopia, the tools, the players' VM's
disk), so a build makes again only the layers its change touched, and a push or a pull moves only
those. The version (`<appVersion>+<commit>`) isn't built into control, the player or the web: their
images tell it to them (`DIRIGENT_VERSION`, and the player's disk), so a commit or a release that
doesn't change their code doesn't rebuild them; only the CLI is built with it. The Rust
dependencies (`dirigent.deps`) change only with `Cargo.lock`. The e2e jobs push the dev shell's
closure to `dirigent.cachix.org` (all of it but cache.nixos.org's: chlive, ClickHouse, Redpanda, …),
which CI, nixbuild.net and your machines then substitute.

## Releasing

A release is a pull request from a `release/X.Y.Z` branch, and the tag `vX.Y.Z` its merge gets:

1. Set `version` and `appVersion` in `chart/dirigent/Chart.yaml` to `X.Y.Z`.
2. Record the release's player logs: with the stack running the release's code, run a few mock
   sessions (turns with tool calls, an interrupted `?demo`, a stop and restart) and save each with
   `dnvr save-log <id> <name>` into `player/logs/X.Y.Z/`.
3. Turn the changelog's **Unreleased** into `## X.Y.Z (date)`, and bump `--version` in `docs/operating/installing.md`.
4. Commit `chore: release X.Y.Z` on `release/X.Y.Z`, push it and open a pull request to master.
5. Approve its `e2e` run (the `e2e` environment's reviewers; on the run's page): `kube` runs
   kube-test on a test cluster and `stack` runs `dnvr e2e`, on GitHub's runners with the Nix builds on
   nixbuild.net. Each uploads what happened (`kube-runs`: kube-test's runs; `stack-logs`).
6. Merge it once both pass. `release.yml` then tags the merge `vX.Y.Z`, and only when it is the
   commit both passed on (rebase the branch on master first if master moved). Tags aren't pushed by
   hand.

The tag's workflows build and push the images and the chart to `ghcr.io/adamchol` (on nixbuild.net;
the chart's version must match the tag), build the CLI on macOS and update the Homebrew formula in `adamchol/homebrew-dirigent`, and build the
CLI with Nix on Linux (x86_64, aarch64) and macOS into the `dirigent.cachix.org` cache.
