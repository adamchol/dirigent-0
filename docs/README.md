# dirigent documentation

dirigent runs headless coding agents ("players") on your own infrastructure and keeps everything
they do in one log. These docs are mostly about using it; the last parts explain how it's built.

New here? Read [Getting started](getting-started.md), then [Concepts](concepts.md).

## Start here

- [Getting started](getting-started.md): run dirigent on your machine with a mock model, make a
  session, follow it locally; then what changes on a real install.
- [Concepts](concepts.md): sessions, players, the log, dirigents, workspaces, where sessions run.

## Using dirigent

- [The web UI](guides/web.md): the sessions list, making a session, the chat, the menus and
  settings.
- [Sessions](guides/sessions.md): making, messaging, steering, interrupting, stopping, sharing,
  renaming, archiving and deleting sessions; models, providers and thinking.
- [The agent](guides/agent.md): what a player does in a turn: its tools and their limits, the system
  prompt and project instructions, compaction, titles, errors and retries.
- [Dirigents](guides/dirigents.md): sessions that direct your other sessions.
- [Accounts and secrets](guides/accounts-and-secrets.md): your Anthropic key, your ChatGPT plan,
  secrets, and access to private GitHub repositories.
- [Workspaces](guides/workspaces.md): sources, checkpoints, the mirror in the log, `dirigent sync`,
  and what happens when a workspace is gone.
- [Hosts](guides/hosts.md): running your sessions on your own machines with `dirigent host`.
- [Importing](guides/importing.md): continuing a Claude Code or Codex session in dirigent.

## Running dirigent

- [Installing on Kubernetes](operating/installing.md): prerequisites, the services it needs,
  sign-in, exposing it, and a first install.
- [Configuration](operating/configuration.md): every value of the Helm chart.
- [Machines and systems](operating/machines-and-systems.md): players' VMs, default machines and
  limits, warm VMs, the cluster's own NixOS system, the shared Nix store.
- [Operations](operating/operations.md): upgrades, logs and status, backups and retention, deleting
  data, troubleshooting.

## Reference

- [CLI](reference/cli.md): every `dirigent` command and flag, files and environment variables.
- [HTTP API](reference/http-api.md): control's API, as the web and the CLI use it.
- [Events](reference/events.md): the log's rows and every event kind.
- [Control](reference/control.md): control's flags and environment variables.

## How it's built

- [Architecture](architecture.md): the components, the log's path, leases, runners and auth.
- [Development](development.md): the dev stack, tests, the test cluster, and releases.
