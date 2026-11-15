# Changelog

dirigent's releases: the chart and the images, both at the chart's version (`chart/dirigent/Chart.yaml`).

## 0.1.11 (2026-11-15)

### Added
- The chart's Services take annotations: `control.service.annotations`, `chlive.service.annotations`, `web.service.annotations`, e.g. to put control's players' service (for hosts) and the web on a tailnet with the Tailscale operator. The installing guide has the recipe.

### Fixed
- The web's chat opened sessions partway up their history, and jumped while a reply streamed. It now keeps to the bottom while you're there, and stays where you scroll to otherwise.

## 0.1.10 (2026-11-15)

### Added
- Documentation: a README, and `docs/` on using dirigent (the web, sessions, the agent, dirigents, accounts and secrets, workspaces, hosts, importing), running it (installing, configuration, machines and systems, operations), references (CLI, HTTP API, events, control) and its architecture.
- Releases' CLI is in the `dirigent.cachix.org` Nix cache (x86_64 and aarch64 Linux, Apple silicon macOS), which the flake names: `nix profile install github:adamchol/dirigent-0/v<version>#cli` downloads it.

### Fixed
- A session that came back to a host it had run on worked on the files the host kept, without what it did elsewhere meanwhile, and mirrored them into the log as its latest. Its player now brings them up to the session's latest files.

## 0.1.9 (2026-11-15)

### Added
- `dirigent import claude-code|codex [<id>]` makes another agent's session a dirigent session that goes on from where it was. Without an id, it takes the last session of the current directory.
  - The conversation goes into the new session's log: turns, replies, tool calls and results, compactions and the title. Thinking is kept to read but never sent to a model again. Tools dirigent doesn't have stay in the history as they were called.
  - The workspace is the project's git remote and branch, with the project's files as they are on your machine on top: uncommitted changes and untracked files too, not ignored ones.
  - What looks like a key or token is redacted, in the conversation and in the files git doesn't have as committed. `--no-redact` keeps it.
  - `--dry-run` shows what would be imported. Importing the same session again says where it went; `--again` makes another session.
  - The session isn't started: your next message starts it.
- Codex sessions are read from Codex's rollout format, which hasn't been tried on a real Codex session yet.
- Hosts: your own machines run your sessions (`dirigent host`).
  - The host connects to dirigent as you, and runs a player for each session you place on it, in a directory of its own under its roots. It's listed while it's connected; nothing about it is stored.
  - In the web: a new session's "Runs on", an idle session's "Runs on" menu, and your hosts in Settings. Through the API: `PUT /sessions/{id}/environment` and `GET /environments`.
  - Its config is `~/.config/dirigent/host.toml`: `url`, `name`, `roots`, `slots`, `labels`, `environment` (added to the players' system prompt, which describes the host's system and user in place of the cluster's NixOS VMs).
  - As a service: `dirigent host install` (a launchd agent on macOS, a systemd user unit on Linux), or `sudo dirigent host install --system [--user <name>]` (a launchd daemon, a systemd system unit). There are modules too: `homeManagerModules`, `nixosModules` and `darwinModules.dirigent-host`.
  - Homebrew: `brew install adamchol/dirigent/dirigent`, built from source; `brew services start dirigent` runs the host.
  - With `hosts.expose`, the chart routes control's players' service (gRPC) at the web's address, so a host needs only `url`. Otherwise, give hosts its address (`players`), e.g. on a tailnet.
- A session moving between the cluster and a host keeps its files: a player that finds the workspace gone, with no checkpoint to restore, gets the files the log has (all but those .gitignore leaves out, and those over 50 MB). The conversation notes the move.

### Fixed
- On ClickHouse before 26.x, 0.1.8's ingestion dropped every event: the message times' UTC text (`2026-11-14T23:37:12.111Z`) is read only with `date_time_input_format=best_effort`, which the pipeline's inserts now set. With `ingestion.enabled: false`, add it to your connector's `clickhouseSettings`. Events lost meanwhile can be ingested again from the topic, while it has them.

### Changed
- Leases are taken with a token: control gives one to each player it starts, for that session alone. A player's id, which a session's log shows, no longer lets anything that reaches control's players' service take the lease. Players and VMs of earlier releases can't take leases from this control (`--reserved-only`, which the chart sets).
- The player is a library too: the CLI runs players itself (`dirigent __player`), so one binary is all a host needs.
- The player's logging target is `dirigent_player` (e.g. `RUST_LOG=dirigent_player=debug`).

### Notes
- An imported session's rows carry the time of the import. The web marks where the imported conversation ends, with the dates it really had.

## 0.1.8 (2026-11-14)

### Changed
- The player comes on its own small disk (image `player`), beside the VM's NixOS disk. A release that changes only dirigent leaves the VM image as it was, so nodes pull a few MB instead of the whole disk.
- A cluster's own system for the VMs is rebuilt only when the VM image changes, not on every dirigent release.
- A failed model call says what the provider said: `API error 400: <its message> (<its code>)`, in place of the raw response.
- A session with no thinking runs at low effort on models that can't stop thinking (Claude Opus 5.5, Sonnet 5.5, Fable), where it failed before.

### Fixed
- Message times in ClickHouse were saved as 9999-12-31 on ClickHouse 26.8, which reads a bare number as seconds. The pipeline now stores them as UTC text, which every version reads the same. This broke the web's message times, its ordering of sessions by activity, and the TTL on `model_delta` rows.
- On a ChatGPT plan, a turn ended early when the model called several tools at once: only the first was run. All of them run now.
- On a ChatGPT plan with GPT-5.6 and later, calls were refused for asking for explicit prompt caching. The session is now the prompt cache key.
- Compaction:
  - The summary quoted the compaction instructions as the user's latest request.
  - After a compaction, Claude's newer models (Haiku, Sonnet and Opus 5.5, and Fable) refused the kept turns' thinking on accounts made on or after 2026-08-31. Kept turns now go without it.
  - Compaction could keep a large tool result and drop almost nothing, then retry after every reply.
  - A reply that isn't a summary (a refusal, or an answer to the user) is no longer taken as one.
- Warm VMs:
  - The pool is refilled once a warm VM is taken, and emptied when none are wanted.
  - Warm VMs are told `DIRIGENT_WARM=true`, which their player accepts.
- A VM booting into a cluster's system mounts the shared store before looking for the system in it.
- Shared-store paths a generation's writer missed are retried, up to 3 times.
- A deleted VM's turn finishes before its shared-store mount goes.
- The tools image's Nix finds its CA bundle, so shared-store writers reach cache.nixos.org.

### Notes
- Warm VMs, per-session machines, booting into a cluster's own system and the shared store have now been tested on a cluster (k3s with KubeVirt).
- If you run ClickHouse 26.8, rows ingested before this release keep the 9999 timestamp. They can be repaired from the topic's message timestamps while the topic still has them (90 days by default). The chart's new pipeline config applies on upgrade; with `ingestion.enabled: false`, add the `ts` transform yourself (see `ingestion.pipelines` in `values.yaml`).

## 0.1.7 (2026-11-12)

### Added
- Each session has its own machine: cores, memory and workspace disk, chosen when it's made in the new-session form's "Machine" section, or by a dirigent's `create_session`. A session that asks for none gets the default machine as it is when the session starts.
- The default machine is the chart's (`player.cpu`, `memory` and `workspaceSize`). With `player.defaultsPerUser`, each user sets their own in Settings, and what they leave out is the chart's.
- `player.limits` caps what a session's machine may have. It has no limits by default.
- Warm VMs (`player.warm.count`, 0 by default) are booted ahead of time, so a starting session on their machine skips the boot:
  - without per-user defaults, that many on the default machine;
  - with them, that many for each user who made a session lately (`player.warm.activeWithin`), on their own machine.

  A session on any other machine, or with none waiting, gets a VM of its own. Warm VMs are replaced on an upgrade, a new default machine, a new players' system or a new shared-store generation.

### Notes
- Warm VMs and per-session machines haven't been verified on a cluster yet.

## 0.1.6 (2026-11-12)

### Added
- The players' VMs come with common tools on the agent's PATH: coreutils and friends, archives, curl, wget, ssh, ripgrep, fd, jq, git, gh, gcc, make, pkg-config and Python. `nixpkgs#…` and `<nixpkgs>` are pinned to the image's nixpkgs. The root disk has about 24 GB for the Nix store.
- The system prompt describes the environment: root on NixOS, `nix shell` for anything missing, `nix develop` for a project's flake, and a venv for Python.
- A cluster's own NixOS configuration for the VMs (`player.nixos.files`):
  - Control builds it in a VM, once per configuration and dirigent version, signs it, and puts it in a new S3 Nix cache (`nix.cache`, bucket `dirigent-nix`).
  - Sessions' VMs boot into it with kexec, kernel and all.
  - While it's building, or after it failed, sessions run the stock system. A system no VM comes up with is marked broken.
  - Progress shows as Events on ConfigMap `dirigent-player-nixos` and at `GET /system`, and the web's Settings say which system new sessions get.
- An optional Nix store all VMs share (`nix.shared`, off by default):
  - What sessions add to their store is put into read-only generations on a shared volume, which later VMs mount under their own.
  - `promote: upstream` takes only upstream-signed paths; `all` takes everything, with a warning in the values about leaks.
  - It needs KubeVirt's `EnableVirtioFsStorageVolumes` feature gate and uses Nix's experimental local-overlay store.

### Fixed
- Files over 50 MB are restored with the workspace. Kopia had a size cap that left them out of checkpoints; it now has none. Files `.gitignore` excludes stay out.

### Notes
- Booting into a cluster's own system and sharing through the shared store haven't been verified end to end on a cluster yet.

## 0.1.5 (2026-11-11)

### Added
- Per-user settings, kept by control. The first is which events the chat shows: the conversation, its activity, or everything.
- A message sent to an idle session starts it.

## 0.1.4 (2026-11-11)

### Added
- Private GitHub repositories, cloned either with the user's `GITHUB_TOKEN` secret or through an optional GitHub App (`github.app`). Users connect the App in the web's account menu. Each session gets a token for its own repository alone, renewed hourly.
- A starting session shows what it's waiting on.

### Changed
- Web sign-ins are held in a cookie-backed session.

## 0.1.3 (2026-11-11)

### Added
- Sessions can be archived and deleted. Deleting removes the session's events from ClickHouse and its checkpoints.
- The log records each player's version. Logs saved at each release are tested to still load.

## 0.1.2 (2026-11-11)

### Fixed
- Tool calls in OpenAI replies that completed are run, and an empty model stream fails the turn.

### Added
- The CLI is a Nix package of its own.

## 0.1.1 (2026-11-11)

### Fixed
- ClickHouse's role DDL no longer names a user directory.

### Changed
- A release tag must match the chart's version, and images are tagged from its appVersion.

## 0.1.0 (2026-11-10)

The first release.

- **Players**:
  - headless coding agents on Anthropic's API (prompt caching, retries with backoff), or on a ChatGPT plan linked through the `dirigent` CLI;
  - read output capped, and the context compacted near the window or on overflow.
- **The log**: every session event goes to Kafka, then to one ClickHouse table ordered by Kafka offset. chlive serves it live.
- **Control**:
  - leases sessions to players, with grants, writing through chlive;
  - starts, stops and restarts players, who hold sessions over a gRPC lease and hand them over when stopped.
- **Sessions**:
  - a spec of model, effort and git source, cloned into the workspace;
  - a generated title and description, which owners can rename and hold;
  - dirigents: sessions that start and steer other sessions.
- **Workspaces**:
  - checkpointed to kopia on S3 after every turn, and restored when gone;
  - mirrored into the log as git bundles after each tool call, which `dirigent sync` follows locally.
- **Access**:
  - Google sign-in with control-signed tokens;
  - per-user Anthropic keys, and sessions' secrets requested by the dirigent, sent on the lease and redacted from the log.
- **Web UI**: watch sessions live, start, stop and message them, with streamed replies, sorted by activity.
- **Kubernetes**:
  - players as KubeVirt VMs, installed by a Helm chart;
  - an optional single-node Garage for S3, and an optional Gateway API HTTPRoute;
  - secrets made at runtime by `control ensure-secrets`;
  - ClickHouse and Postgres migrations by openmig.
- **CI**: images built on nixbuild.net and pushed, with the chart, to ghcr.
