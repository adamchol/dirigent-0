# Hosts

A **host** is a machine of yours that runs your sessions: a laptop, a workstation, a server, a Mac
for iOS work. You run `dirigent host` on it; it connects to dirigent as you, and runs a player for
each session you place on it, in a directory of its own. The session works there with the host's
tools, and moves back to the cluster (or to another host) with its conversation and files.

Hosts are listed while they're connected, and nothing about them is stored: a host that's off is
simply not there.

## What a host's sessions can do

A host's players run **as the host's user** (the one running `dirigent host`), as ordinary
processes. The agent's file tools stay inside the session's workspace, but its Bash runs any
command that user can. Run a host as a user whose files and credentials you're happy for your
agents to reach, or give it a dedicated user (`--system --user`, below).

## Requirements

- The `dirigent` CLI, signed in as you (`dirigent login <url>`).
- `git` and `bash` on the host's `PATH` (players need them). The service installers keep the
  `PATH` you install them with.
- The host must reach three things of the install: its API (`/api`), chlive (`/live`), both
  through the web's address, and **control's players' service** (gRPC). The last one isn't public
  by default; the install exposes it one of two ways:
  - **`hosts.expose: true`** in the chart: it's routed at the web's address, and the host needs only
    the web's address. Ask your operator (see
    [Configuration](../operating/configuration.md#hosts)).
  - **A private address**, for instance control's Service on your tailnet (see [On a
    tailnet](../operating/installing.md#on-a-tailnet)): put it in the host's config as `players`.

## Installing

### Homebrew (macOS, or Linux with Homebrew)

```sh
brew install adamchol/dirigent/dirigent
dirigent login https://dirigent.example.com
brew services start dirigent            # runs `dirigent host run` as a launchd agent
```

Its log is `$(brew --prefix)/var/log/dirigent-host.log`.

### As a service, from the CLI

```sh
dirigent host install                   # a launchd agent (macOS) or a systemd user unit (Linux)
dirigent host status
dirigent host uninstall
```

| | File | Log |
|---|---|---|
| macOS | `~/Library/LaunchAgents/dev.dirigent.host.plist` | `~/Library/Logs/dirigent-host.log` |
| Linux | `~/.config/systemd/user/dirigent-host.service` | `journalctl --user -u dirigent-host` |

On Linux, a user unit stops when you log out unless lingering is on: `loginctl enable-linger`.

### For the whole machine (`--system`)

```sh
sudo dirigent host install --system [--user <name>] [--credentials <file>]
sudo dirigent host status --system
sudo dirigent host uninstall --system
```

A launchd daemon (`/Library/LaunchDaemons/dev.dirigent.host.plist`, log
`/var/log/dirigent-host.log`) or a systemd system unit (`/etc/systemd/system/dirigent-host.service`).
It starts at boot, without anyone logged in.

- It copies your CLI credentials to `/etc/dirigent/credentials.json` (from `--credentials`, or from
  `$SUDO_USER`'s `~/.config/dirigent/credentials.json`), so the host connects as you.
- Its config is `/etc/dirigent/host.toml` (written if missing), and its sessions go in
  `/var/lib/dirigent` by default.
- `--user <name>` runs it as that user (and gives them those files); otherwise it runs as root.

### Nix modules

The flake has a module for each way:

| Module | Runs as |
|---|---|
| `homeManagerModules.dirigent-host` | You: a systemd user service (Linux) or a launchd agent (macOS) |
| `nixosModules.dirigent-host` | A systemd system service, as `user` or root |
| `darwinModules.dirigent-host` | A launchd daemon (nix-darwin), as `user` or root |

```nix
{
  inputs.dirigent.url = "github:adamchol/dirigent-0";
  # home-manager:
  imports = [ inputs.dirigent.homeManagerModules.dirigent-host ];
  services.dirigent-host = {
    enable = true;
    url = "https://dirigent.example.com";
    roots = [ "~/dirigent" ];
    slots = 2;
    labels = [ "xcode" ];
    environment = "Xcode 26 is installed; simulators with xcrun simctl.";
    path = with pkgs; [ git bash coreutils gnumake ];
  };
}
```

| Option | Type | Default |
|---|---|---|
| `enable` | bool | `false` |
| `package` | package | this flake's CLI |
| `url`, `api`, `chlive`, `players`, `name` | string or null | null (see the config below) |
| `roots` | list of strings | `[]` (the CLI's default) |
| `slots` | positive int or null | null (2) |
| `labels` | list of strings | `[]` |
| `environment` | lines or null | null |
| `playerArgs` | list of strings | `[]` |
| `path` | list of packages | `[ git bash coreutils ]`, put first on the players' `PATH` |
| `user` (NixOS, nix-darwin) | string or null | null (root) |
| `credentialsFile` (NixOS, nix-darwin) | string | `/etc/dirigent/credentials.json` |

Releases are built into the `dirigent.cachix.org` cache. Modules don't use the flake's own settings, so add it to your system's Nix (`nix.settings.extra-substituters = [ "https://dirigent.cachix.org" ];` and `extra-trusted-public-keys = [ "dirigent.cachix.org-1:9st/693o4bC8VbqMDHIoYcYn6Mp4PR/zLNCJ7YSI+GQ=" ];`), and use a release's tag (`github:adamchol/dirigent-0/v<version>`) without making its `nixpkgs` follow yours, or the CLI is built rather than downloaded.

With home-manager, sign in once with `dirigent login`. With the system modules, copy your
credentials: `sudo install -m 600 ~/.config/dirigent/credentials.json /etc/dirigent/credentials.json`
(and `chown` it to `user`, if set).

### By hand

`dirigent host run` runs the host in the foreground (`RUST_LOG=dirigent=debug` for more).

## Configuration

`~/.config/dirigent/host.toml` (`/etc/dirigent/host.toml` with `--system`; `$DIRIGENT_HOST_CONFIG`
overrides). Every key is optional; unknown keys are an error.

```toml
url = "https://dirigent.example.com"   # the web's address; /api, /live and the players' service from it
# players = "http://dirigent-control.tailnet.ts.net:8091"  # control's players' service, if elsewhere
name = "adams-macbook"                 # how you'll see it; default: the hostname
roots = ["~/dirigent"]                 # where sessions' directories go
slots = 2                              # sessions at once
labels = ["xcode", "arm64"]            # shown with it in the web
environment = """
Xcode 26 and the iOS simulators are installed (xcrun simctl).
Use the `ios` scheme to build.
"""                                    # added to the agent's description of where it runs
```

| Key | Default |
|---|---|
| `url` | The address you signed in to (`dirigent login`). |
| `api`, `chlive`, `players` | From `url`: `<url>/api`, `wss://…/live`, `<url>`. Needed if you signed in with `--control`. |
| `name` | `hostname -s`. Your hosts' names are what you pick in **Runs on**; a second host with the same name replaces the first. |
| `roots` | `~/dirigent` (`/var/lib/dirigent` with `--system`). A session's directory is reused in whichever root has it; new ones go in the first. |
| `slots` | 2. A start beyond it is refused ("the host is full"). |
| `labels` | None. |
| `environment` | None. The agent is always told it runs as `<user>` on `<name>`, a `<system>` machine of yours. |
| `player_args` | None: extra flags for the players. |

The host reconnects by itself (backing off up to 30 seconds) when the connection drops.

## Running sessions on it

- **New sessions**: the new-session form's **Runs on** lists your connected hosts.
- **Existing sessions**: an idle session's menu → **Runs on**. The move applies at its next start.
- **API**: `PUT /sessions/{id}/environment {"host": "adams-macbook"}` (`null` for the cluster), or
  `environment` when making it.

Starting a session placed on a host that isn't connected fails with *the session runs on name, which
isn't connected*; it stays idle until the host is back or you move it.

Each session gets `<root>/<session-id>/` with:

| Path | What |
|---|---|
| `workspace/` | The workspace |
| `state/` | The player's state (its mirror repository) |
| `environment.md` | What the agent is told about this host |
| `player.log` | The player's log |

Players outlive the host process: restarting or upgrading `dirigent host` (or uninstalling it)
leaves running sessions running. Each player is started with a clean environment: `PATH`, `HOME`,
`USER`, `LOGNAME`, `SHELL`, `TMPDIR`, `LANG`, `LC_ALL`, `TZ` and `RUST_LOG` from the host, plus
`ANTHROPIC_BASE_URL` and `OPENAI_BASE_URL` if set. Your keys and secrets come from control, as in the
cluster.

When a session is deleted, the host deletes its directory (at once if connected, otherwise when it
next connects), unless its player is still running. A session that moves away keeps its directory
on the host.

## Moving between the cluster and hosts

A session's conversation is in the log, and so are its files ([the
mirror](workspaces.md#the-mirror-in-the-log)). Moved to a host, its player clones the source and
checks out the session's latest files; moved back, the same happens in a VM. Ignored files and files
over 50 MB don't move. The agent is told the session moved, and where it runs now (its environment
in the system prompt changes too).

A session that comes back to a host it ran on before finds its old directory there; its player
brings the files up to the session's latest, so what changed elsewhere meanwhile is there, and what
was deleted is gone. Ignored files (build outputs, dependencies) are as the host left them.

Sessions on hosts aren't checkpointed: their workspace is the host's directory, kept as long as the
session is there.
