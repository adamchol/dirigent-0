# CLI

`dirigent` signs you in, links the accounts that pay for your sessions, manages secrets, follows
workspaces, imports other agents' sessions, and runs your machines as hosts.

## Installing

| How | Command |
|---|---|
| Homebrew (macOS, Linux) | `brew install adamchol/dirigent/dirigent` (built from source; `--HEAD` for master) |
| Nix | `nix profile install github:adamchol/dirigent-0/v<version>#cli`, or `nix run github:adamchol/dirigent-0/v<version>#cli -- …`. Releases are in the `dirigent.cachix.org` cache, which the flake names: accept it when Nix asks (or pass `--accept-flake-config`) to download the CLI instead of building it. |
| From source | `cargo install --path cli` in a clone (needs `git` at run time) |

`dirigent --version` prints its version.

## Commands

### Signing in

| Command | |
|---|---|
| `dirigent login <url>` | Sign in to the dirigent at `<url>` (the web's address). With Google sign-in, opens your browser (`--no-browser` prints the URL instead) and waits up to 5 minutes. |
| `dirigent login <url> --as <subject>` | On a development install (declared auth): who you are, default `user:dev`. |
| `dirigent login <url> --control` | `<url>` is control's own API, not the web's (`/api` isn't added). Then `sync`, `import` and `host` need chlive's and the players' addresses given explicitly. |
| `dirigent whoami` | Who you are, where, the dirigent's version and this CLI's, your ChatGPT link and Anthropic key. |
| `dirigent version` | This CLI's version and what the dirigent runs: control (version, start, runner), the players' VM image and system, Postgres, ClickHouse, the install's pods in Kubernetes, and your connected hosts. `--json` for bug reports. (`dirigent --version`: the CLI's alone.) |
| `dirigent logout` | Sign out: revokes the Google refresh token and deletes the credentials. |

### Accounts

| Command | |
|---|---|
| `dirigent link anthropic` | Give your Anthropic API key (prompted, hidden; or from stdin). Checked with Anthropic first. |
| `dirigent unlink anthropic` | Remove it. |
| `dirigent link chatgpt [--no-browser]` | Link your ChatGPT plan: sign in to OpenAI in the browser. |
| `dirigent unlink chatgpt` | Unlink it (revoked at OpenAI). |
| `dirigent models` | Your ChatGPT plan's models: slug and name. |

### Secrets

| Command | |
|---|---|
| `dirigent secret set <NAME> [--session <id>]` | Set a secret for all your sessions, or for one. The value is prompted (hidden) or read from stdin. |
| `dirigent secret list [--session <id>]` | Names only. |
| `dirigent secret rm <NAME> [--session <id>]` | |

See [Accounts and secrets](../guides/accounts-and-secrets.md#secrets).

### Workspaces

| Command | |
|---|---|
| `dirigent sync <session> [dir]` | Write the session's files into `dir` (default `./<title>`) and keep them in step until Ctrl-C. |
| `  --once` | Sync once and exit. |
| `  --chlive <url>` | chlive's address (`$DIRIGENT_CHLIVE`); by default from your sign-in (`wss://<host>/live`). |

See [Workspaces](../guides/workspaces.md#following-a-workspace-dirigent-sync).

### Importing

`dirigent import claude-code|codex [session]` with `--dir`, `--model`, `--effort`, `--provider`,
`--dry-run`, `--again`, `--no-redact`, `--chlive`. See [Importing](../guides/importing.md).

### Hosts

| Command | |
|---|---|
| `dirigent host run` | Run the host in the foreground. |
| `dirigent host install` | Install and start it as your service: a launchd agent in your login session (macOS), or a lingering systemd user unit (Linux). As you, never with `sudo`. |
| `dirigent host uninstall` | Stop and remove it. Running players go on. |
| `dirigent host status [--json]` | What runs here: its service (whichever installed it), whether dirigent has it connected, the sessions running here and the directories kept, the last warnings and errors in its log. |

See [Hosts](../guides/hosts.md).

## Files

| File | |
|---|---|
| `~/.config/dirigent/credentials.json` | Your sign-in: the API's address and a Google refresh token (or, declared, your subject). Mode 0600. `$DIRIGENT_CREDENTIALS` overrides; `$XDG_CONFIG_HOME` is honored. |
| `~/.config/dirigent/host.toml` | The host's config ([Hosts](../guides/hosts.md#configuration)). `$DIRIGENT_HOST_CONFIG` overrides. |
| `~/.cache/dirigent/sync/` | `sync`'s git repositories, one per session and folder (`$XDG_CACHE_HOME` is honored). |

## Environment

| Variable | |
|---|---|
| `DIRIGENT_CREDENTIALS` | The credentials file. |
| `DIRIGENT_HOST_CONFIG` | The host's config. |
| `DIRIGENT_CHLIVE` | chlive's address for `sync` and `import`. |
| `CLAUDE_CONFIG_DIR`, `CODEX_HOME` | Where `import` finds Claude Code's and Codex's sessions. |
| `RUST_LOG` | Logging, for `host run` (default `dirigent=info`). |

## How it authenticates

- **Google sign-in**: `login` runs Google's sign-in for installed apps (PKCE, a redirect to a port on
  127.0.0.1), with the client the install hands out at `GET /api/auth`. It keeps the refresh token
  and sends a fresh Google ID token with each request (`authorization: Bearer …`), renewing it
  before it expires.
- **Declared**: it sends `dirigent-user: <subject>`.

ChatGPT linking uses the same local redirect: the CLI is the only place it can happen.
