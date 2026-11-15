# Architecture

How dirigent is built, from the top down to the level you need to reason about its behavior. For
using it, see the guides; for code-level detail, the crates' module docs.

## The idea

**The log is the session.** Every input and everything an agent does is an event appended to one
log per session. Nothing else holds a conversation: a player is a process that reads a log, acts,
and appends to it. So any player, anywhere, can take over a session at any moment, and anyone with
access can watch it live or read it later, in full.

Around the log, dirigent keeps as little state as it can: who may use which session, the players'
leases, users' keys and settings. Whatever can be derived (a session's state, its title, which
hosts are up) is derived, not stored.

## Components

```
                         ┌──────────────────────── control ────────────────────────┐
  web (Bun) ── /api ────▶│ HTTP API · auth · sessions & access · keys & secrets     │──▶ Postgres
  CLI ───────────────────│ players' service (gRPC): leases, warm VMs, hosts         │   (CloudNativePG)
  dirigents ─────────────│ runners: KubeVirt VMs · local processes · hosts          │
      │                  └──────────┬───────────────────────────▲───────────────────┘
      │ /live                       │ starts                    │ lease stream (tokens, stop)
      ▼                             ▼                           │
   chlive ◀──── reads & writes ── players (one per session) ────┘
   (WebSocket;                         │
    grants from Postgres)              │ inserts
      │  ▲                             ▼
      │  └── history ── ClickHouse ◀── Kafka Connect sink ◀── Kafka topic `session_events`
      └────── live tail ─────────────────────────────────────────────┘
```

| Component | Role |
|---|---|
| **player** (`player/`) | The agent. One process per session: holds the lease, rebuilds the conversation from the log, runs turns, writes every event. |
| **control** (`control/`) | The control plane: the HTTP API, sign-in, sessions and access, users' keys, secrets and settings (Postgres, of which it's the only writer); leases sessions to players over gRPC and starts players through a runner. |
| **chlive** ([dialohq/chlive](https://github.com/dialohq/chlive)) | Live queries over Kafka and ClickHouse, over WebSockets: a query's history from ClickHouse, then its live tail from Kafka. Also the write path into the topic. Enforces who may read and write each session. |
| **The ingestion** (durable ClickHouse sink) | Kafka Connect loading the topic into ClickHouse exactly once; its pause server lets chlive snapshot a table at a known point. |
| **web** (`web/`) | The UI, and a small server proxying `/api` to control and `/live` to chlive. |
| **CLI** (`cli/`) | Sign-in, linking accounts, secrets, `sync`, `import`, and `host`, which runs players on users' machines (the player is a library inside it). |
| **protocol** (`protocol/`) | The events, how a log reads as a conversation (`fold`), the row format and queries, and control's types and gRPC protocol. |
| **sync** (`sync/`) | A workspace as git commits carried in the log: written by players, checked out by the CLI. |

## The log's path

**Writing.** Players, users (through the web or CLI) and dirigents insert rows through chlive,
signed in with a token. chlive checks the writer may insert that kind into that session (owners
write inputs, players write anything, viewers nothing), sets the row's `author`, and produces it to
the Kafka topic `session_events`, keyed by session, so a session's events keep their order in one
partition.

**Loading.** Kafka Connect's ClickHouse sink inserts the topic into `dirigent.session_events`,
exactly once: it keeps its progress in a KeeperMap table, in step with what it inserted, and adds
each message's partition, offset and timestamp. The table is ordered by `(session_id, offset)`.

**Reading.** A reader subscribes to a query (a session's events, say). chlive answers from a
snapshot of the ClickHouse table, made while the pause server holds ingestion at a known offset,
then follows Kafka from that offset: no gap, no duplicate. The web, players, dirigents and `sync`
all read this way.

**Grants.** chlive learns who may read and write what from Postgres, by logical replication of two
tables control keeps: `session_access` (owners, viewers, players, importers) and `insert_kinds`
(which kinds each may write). A grant control writes is in force at chlive within moments; a player
waits for its own before writing.

## Sessions and leases

control keeps sessions in Postgres: the spec, the owner and viewers, the epoch, archival, the
placement. A session's **lease** is its `player` row in `session_access`; holding it is what lets a
player write to the session.

- **Starting.** control *reserves* the lease for a new player id (the next epoch, expiring after
  `startup`), signs a **lease token** for that player and session, and starts the player through a
  runner, which hands it the token. The player calls `Lease` on control's gRPC service with its id,
  epoch and token; control checks them and gives it the lease, the session's spec, and its
  credentials.
- **Holding.** The lease is a gRPC stream. Over it control sends fresh credentials (chlive and
  control tokens, the owner's model credentials, the session's secrets) when they change or near
  expiry, and stop requests. HTTP/2 keepalives detect a dead player; a dropped stream leaves the
  lease for a grace period, in which the player can come back with its epoch.
- **Ending.** A player that stops gives the lease back. A player that was stopped from outside (its
  VM deleted) hands over, and control starts the next one at once. A sweep every second finds
  leases whose player never came or didn't come back, drops them, and starts another player.
- **Only control's players.** With `--reserved-only` (the chart's setting), a lease is given only
  with a token control signed for that player and session, so the players' service can be reached
  from outside (users' hosts) without letting anyone take a session.

## Runners

A runner starts and forgets players. control has one for the cluster, and one per connected host.

- **KubeVirt** (`control/src/runner.rs`): a `VirtualMachineInstance` per player, from the chart's
  template, sized by the session's machine; the player's environment goes in as cloud-init data.
  Warm VMs are VMIs whose player waits (`Players.Wait`) to be assigned a session; control's pool
  keeps them at count and replaces those that no longer match. For a cluster's own NixOS system,
  control runs a build VM, and session VMs kexec into the result. See [Machines and
  systems](operating/machines-and-systems.md).
- **Local** (development): a child process per player, in a directory per session.
- **Hosts** (`control/src/hosts.rs`): a user's `dirigent host` holds a `Host` gRPC stream, signed
  in as the user; control sends it `Start` (session, player, epoch, token) and `Forget`, and it runs
  `dirigent __player` processes. Hosts live only in control's memory, by user and name, while
  connected. A session's placement (`sessions.environment`) chooses the runner at each start.

## Inside a player

1. Take the lease (or wait for an assignment, when warm).
2. Write `player_attached`, then subscribe to the session's log: history, then live.
3. Prepare the workspace: restore a checkpoint, or clone the source; then bring it up to date from
   the log's mirror.
4. **Fold** the log into a conversation (`protocol/src/fold.rs`): turns, compactions, interrupted
   turns closed, queued inputs.
5. Loop: take inputs as they arrive; run turns (model calls through the provider, tools, steering,
   interrupts, compaction); after every mutating tool, commit the workspace into the log; after every
   turn, checkpoint it.
6. On stop: finish or cancel the turn, write `player_detached`, give the lease back.

Every event the player produces is acknowledged by chlive before it moves on (streamed deltas
excepted), so the log is never behind what the agent did.

## Workspaces

Two mechanisms, for two needs:

- **Checkpoints** (kopia, in S3) keep the whole workspace, files of any size, deduplicated, after
  every turn: the way back after a VM is gone.
- **The mirror** (`sync/`) is a git repository of the player's own, beside the workspace (whose own
  `.git` it never touches). After every change it commits the workspace's files (ignored files and
  files over 50 MB aside) and writes the new objects into the log as git bundles. Readers rebuild the
  repository from the bundles: `dirigent sync` follows a workspace live, and a player on a new
  machine checks the latest files out, even without a checkpoint.

## Auth

- **Users** sign in with Google. control verifies Google ID tokens; the web exchanges one for an
  HttpOnly sign-in cookie, from which it gets short-lived **control tokens**. The CLI sends Google
  ID tokens itself.
- **control tokens** are Ed25519 JWTs control signs (issuer `dirigent-control`), for control's API
  and chlive (which verifies them with control's JWKS). Players get theirs on the lease, as
  `player:<id>`.
- **Dirigents** act for their owner: their lease carries a token with the owner as subject and the
  player as the actor (`act`), so what they do is the owner's, and marked as theirs.
- **Lease tokens** (audience `lease:<session>`) let a player take its session's lease, as above.
- **Users' secrets** (API keys, ChatGPT refresh tokens, secrets) are sealed with XChaCha20-Poly1305
  under the install's link key, in Postgres; players get them only in memory, over the lease.

## Storage

| Store | Holds |
|---|---|
| Kafka `session_events` | Every event, for its retention (90 days by default) |
| ClickHouse `dirigent.session_events` | Every event, for good (streamed deltas: a day) |
| Postgres `dirigent` schema | Sessions, access and leases, users' keys, links, secrets, settings, web sign-ins, GitHub installations, players' systems, the shared store's paths |
| S3 | Checkpoints (kopia), the players' Nix cache |
| The shared Nix store volume | Generations of Nix paths VMs added |

Schemas live in `db/` as openmig declarations and migrations, applied by control's init container.
