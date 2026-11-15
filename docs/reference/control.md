# Control

control's command line: flags, and the environment variables they read. The chart sets these; you
need this page to run control yourself, or to tune it through the chart's `control.env`.

```
control [flags] [-- <player args>]
control keygen <dir>
control ensure-secrets [--garage] [--keys <dir>]
```

Logging: `RUST_LOG` (default `control=info`).

## Listening and storage

| Flag | Environment | Default | |
|---|---|---|---|
| `--listen` | `DIRIGENT_CONTROL_LISTEN` | `127.0.0.1:8090` | The HTTP API. |
| `--grpc-listen` | `DIRIGENT_CONTROL_GRPC_LISTEN` | `127.0.0.1:8091` | The players' service (gRPC). |
| `--postgres` | `DIRIGENT_CONTROL_POSTGRES` | required | Postgres URL. |

## Auth and tokens

| Flag | Environment | Default | |
|---|---|---|---|
| `--auth` | | `declared` | `declared` (callers name themselves) or `oidc`. |
| `--signing-key` | `DIRIGENT_SIGNING_KEY` | required | Ed25519 key (PEM) control signs tokens with. |
| `--issuer` | `DIRIGENT_ISSUER` | `dirigent-control` | Its tokens' issuer. |
| `--token-ttl` | | `600` | Seconds a control token lasts (renewed halfway). |
| `--session-days` | `DIRIGENT_SESSION_DAYS` | `30` | Days a web sign-in lasts unused. |
| `--oidc-issuer` | `DIRIGENT_OIDC_ISSUERS` | | Accepted ID-token issuers (comma-separated in the variable). |
| `--oidc-audience` | `DIRIGENT_OIDC_AUDIENCES` | | Accepted audiences; the first is the web's client. |
| `--oidc-jwks` | `DIRIGENT_OIDC_JWKS` | Google's | Where the issuer's keys are. |
| `--oidc-subject-claim` | | `email` | |
| `--oidc-subject-prefix` | | `user:` | |
| `--oidc-required-claim` | `DIRIGENT_OIDC_REQUIRED_CLAIMS` | | `k=v` claims a token must have. |
| `--oidc-cli-client-id`, `--oidc-cli-client-secret` | `DIRIGENT_OIDC_CLI_CLIENT_ID`, `_SECRET` | | The CLI's OAuth client, handed out at `GET /auth`. |

## Players and runners

One runner: local processes (`--players`) or KubeVirt (`--kube-template`).

| Flag | Environment | Default | |
|---|---|---|---|
| `--players` | `DIRIGENT_PLAYERS` | | Run players as local processes, in `<dir>/<session>/`. |
| `--player` | | `player` | The player program (local runner). |
| `--kube-template` | `DIRIGENT_KUBE_TEMPLATE` | | A VMI template (JSON): run players as KubeVirt VMs. |
| `--kube-namespace` | `DIRIGENT_KUBE_NAMESPACE` | its own | |
| `--kube-name` | `DIRIGENT_KUBE_NAME` | `dirigent` | Its VMs' `dirigent.dev/runner` label. |
| `--reserved-only` | `DIRIGENT_RESERVED_ONLY` | off | Lease sessions only to players control started, with their lease tokens. Set this whenever the players' service is reachable by anything but your players. |
| `--startup` | | `60` | Seconds a started player has to take its lease. |
| `--grace` | | `10` | Seconds a disconnected player keeps its session. |
| `-- <args>` | | | Extra arguments for local players, e.g. `--mock`. |

## Machines and warm VMs

| Flag | Environment | Default | |
|---|---|---|---|
| `--machine-cpu`, `--machine-memory`, `--machine-disk` | | `2`, `4`, `20` | The default machine (local runner; KubeVirt's comes from the template). |
| `--max-cpu`, `--max-memory`, `--max-disk` | `DIRIGENT_MAX_CPU`, `_MEMORY`, `_DISK` | none | Limits on sessions' machines. |
| `--defaults-per-user` | `DIRIGENT_DEFAULTS_PER_USER` | off | Users set their own default machines. |
| `--warm` | `DIRIGENT_WARM` | `0` | Warm VMs (per active user with per-user defaults). |
| `--warm-active-within` | `DIRIGENT_WARM_ACTIVE_WITHIN` | `604800` | Seconds. |

## The players' system and Nix

| Flag | Environment | Default | |
|---|---|---|---|
| `--nixos-config` | `DIRIGENT_NIXOS_CONFIG` | | The cluster's NixOS configuration's directory. |
| `--nixos-configmap` | `DIRIGENT_NIXOS_CONFIGMAP` | `dirigent-player-nixos` | Where build Events go. |
| `--nixos-build-timeout` | `DIRIGENT_NIXOS_BUILD_TIMEOUT` | `3600` | |
| `--kube-build-template` | `DIRIGENT_KUBE_BUILD_TEMPLATE` | | The build VM's template. |
| `--player-build` | `DIRIGENT_PLAYER_BUILD` | | The build program, run locally (development). |
| `--nix-store` | `DIRIGENT_NIX_STORE` | | The shared store's mount. |
| `--kube-writer-template` | `DIRIGENT_KUBE_WRITER_TEMPLATE` | | The generation writer's Job template. |
| `--nix-promote` | `DIRIGENT_NIX_PROMOTE` | `upstream` | |
| `--nix-every` | `DIRIGENT_NIX_EVERY` | `3600` | |
| `--nix-keep` | `DIRIGENT_NIX_KEEP` | `3` | |

## Users' accounts

| Flag | Environment | Default | |
|---|---|---|---|
| `--link-key` | `DIRIGENT_LINK_KEY` | | The key users' API keys, ChatGPT links and secrets are encrypted with. Without it, none can be kept. |
| `--anthropic-api` | `DIRIGENT_ANTHROPIC_API` | `https://api.anthropic.com` | Where keys are checked. |
| `--openai-auth`, `--openai-api` | `DIRIGENT_OPENAI_AUTH`, `DIRIGENT_OPENAI_API` | OpenAI's | ChatGPT linking. |
| `--github-app-slug`, `--github-app-client-id`, `--github-app-client-secret`, `--github-app-key` | `DIRIGENT_GITHUB_APP_SLUG`, `_CLIENT_ID`, `_CLIENT_SECRET`, `_KEY` | | The GitHub App: all or none. `_KEY` is the private key's path. |
| `--github-api`, `--github-web` | `DIRIGENT_GITHUB_API`, `DIRIGENT_GITHUB_WEB` | GitHub's | |

## Deleting and checkpoints

| Flag | Environment | Default | |
|---|---|---|---|
| `--clickhouse-url` | `DIRIGENT_CLICKHOUSE_URL` | | Where deleted sessions' events are deleted. Without it, sessions can't be deleted. |
| `--clickhouse-user`, `--clickhouse-password` | `DIRIGENT_CLICKHOUSE_USER`, `_PASSWORD` | `dirigent_control` | |
| `--kopia-token` / `--kopia-token-secret` | `DIRIGENT_KOPIA_TOKEN` / `DIRIGENT_KOPIA_TOKEN_SECRET` | | The checkpoints' repository (a token, or a Secret with it); for deleting checkpoints and maintenance. |
| `--kopia-state` | `DIRIGENT_KOPIA_STATE` | | kopia's config and cache. |

## Subcommands

- `control keygen <dir>` writes a signing key (`key.pem`, if missing) and its `jwks.json`.
- `control ensure-secrets [--garage] [--keys <dir>]` makes the chart's Secrets in its namespace,
  adding missing keys and never replacing a value, and writes control's own keys to `<dir>` (the
  chart runs it as an init container).
