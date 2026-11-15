# dirigent

Headless coding agents on your own infrastructure. You give a session a repository and a task; a
player (the agent) works on it in a VM of its own, and you follow, steer or stop it from a browser,
the command line, or another agent. Everything a session does is an event in one log, so it can be
watched live, replayed, resumed anywhere, and nothing is lost when a machine goes away.

dirigent is a personal project, pre-1.0, used by its author. It works end to end, but expect rough
edges and breaking changes between releases (see the [changelog](CHANGELOG.md)).

## What it does

- **Sessions that keep going on their own.** A session is a conversation with an agent and a
  workspace (a git clone, or nothing). Its player runs turns until the work is done, with Bash, file
  reading, writing and editing, and search, then waits for your next message. You can message it
  mid-turn to steer it, interrupt a turn, or stop it.
- **Agents that direct agents.** A *dirigent* session has no workspace: it makes, messages, reads,
  starts and stops your other sessions, follows them as they finish, and asks you for secrets
  they need.
- **Your models, your bill.** Sessions run on Claude with your Anthropic API key, or on your
  ChatGPT plan.
- **A VM per session, sized per session.** On Kubernetes, each player gets a KubeVirt VM (NixOS)
  with the cores, memory and disk its session asked for; warm VMs let starts skip the boot; the
  cluster can have its own NixOS system for them.
- **Or your own machines.** `dirigent host` lets your laptop or server run your sessions (for
  example, a Mac for iOS work), and a session moves between the cluster and your machines with its
  files.
- **Workspaces that survive.** The workspace is checkpointed after every turn and mirrored into the
  log after every change; `dirigent sync` follows it on your machine as a plain folder.
- **Bring your history.** `dirigent import` turns a Claude Code or Codex session into a dirigent
  session that goes on from where it was, with the project's files.
- **Private repositories and secrets.** A GitHub App (or your own token) gives each session access
  to its repository alone; secrets reach the agent's tools and are redacted from the log.

## How it works

```
  browser ──┐                     ┌──────────── control ─────────────┐
  CLI ──────┼── web (/api, /live) ┤  sessions, access, leases, keys  │── Postgres
  dirigents ┘          │          └──────┬─────────────────┬─────────┘
                       │                 │ starts          │ leases (gRPC)
                       ▼                 ▼                 │
                    chlive ◀──────── players (KubeVirt VMs, or `dirigent host`)
              (live log, grants)        writes every event │
                       │                                   ▼
                       └────── Kafka ──▶ ClickHouse ◀── Kafka Connect (exactly once)
```

- **The log** is the source of truth: every message, reply, tool call, checkpoint and workspace
  change is a row in one Kafka topic, loaded exactly once into ClickHouse.
  [chlive](https://github.com/dialohq/chlive) serves it live over WebSockets (history from
  ClickHouse, the tail from Kafka) and checks who may read and write each session.
- **control** keeps the little state that isn't the log (sessions, who may use them, users' keys and
  secrets, settings) in Postgres, and leases each session to one player at a time.
- **A player** takes a session's lease, rebuilds the conversation from the log, and works; another
  player can take over at any time from the same log.

[Architecture](docs/architecture.md) explains it in more depth.

## Getting started

- **Try it on your machine** with the development stack and a mock model (no API key, no
  Kubernetes): [Getting started](docs/getting-started.md).
- **Install it on a cluster**: [Installing on Kubernetes](docs/operating/installing.md).
- **Install the CLI**: `brew install adamchol/dirigent/dirigent`, or
  `nix run github:adamchol/dirigent-0#cli -- --help`; see [the CLI](docs/reference/cli.md).

## Documentation

The [documentation](docs/README.md) covers using dirigent (the web UI, sessions, the agent,
dirigents, accounts and secrets, workspaces, hosts, importing), running it (installing,
configuration, machines and systems, operations), references (CLI, HTTP API, events, control),
and the architecture.

## Repository

| Path | What |
|---|---|
| [`player/`](player) | The agent: one process per session, its tools, prompts and providers |
| [`control/`](control) | The control plane: HTTP API, players' gRPC service, runners, Postgres |
| [`protocol/`](protocol) | Events, how a log reads, and control's types and gRPC protocol |
| [`sync/`](sync) | A workspace as git commits carried in the log |
| [`cli/`](cli) | `dirigent`: sign-in, linking accounts, secrets, sync, import, hosts |
| [`web/`](web) | The web UI and its server (Bun, React) |
| [`chart/dirigent/`](chart/dirigent) | The Helm chart |
| [`db/`](db) | Postgres and ClickHouse schemas and migrations (openmig) |
| [`nix/`](nix) | Packages, images, the players' VM, the dev stack, kube-test, Nix modules, Homebrew |

## Development

`nix develop`, then `dnvr up` runs the whole stack locally; see [Development](docs/development.md)
for the stack, the tests and releases.
