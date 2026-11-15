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
agents to reach, or make a dedicated user for it and run it as them. A host never runs as root.

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

A host is always **a user's service**, signed in with that user's own `dirigent login`
(`~/.config/dirigent/credentials.json`, which it renews in place), and run with `sudo` never:

- **macOS**: a launchd agent, in your login session. That's where Xcode's signing identities (your
  login keychain) and the Simulator are; a daemon outside it can't use them. A locked screen doesn't
  stop it; logging out does, and after a restart it's back once you log in: turn on automatic login
  (System Settings → Users & Groups) to have it back by itself. With FileVault, a restart waits for
  your password at boot; for planned restarts, `sudo fdesetup authrestart` skips that once.
- **Linux**: a systemd user unit, lingering, so it runs from boot whether you're logged in or not
  (`dirigent host install` turns lingering on: `loginctl enable-linger`). On NixOS, the module is a
  system unit running as the user you name.

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

On macOS, install it from a terminal on the Mac's screen, not over SSH: launchd starts agents in a
login session.

### What runs, and where: `dirigent host status`

```
Service
  launchd agent dev.dirigent.host (dirigent host install): running, pid 4312
    file ~/Library/LaunchAgents/dev.dirigent.host.plist
    log  ~/Library/Logs/dirigent-host.log
Connection
  adams-macbook: connected, dirigent 0.1.12+abc1234
Sessions running here: 1 of 2
  3f2a…  github.com/you/app  pid 5120, 01:12:03, as adam  ~/dirigent/3f2a…/workspace
Workspaces kept (no player running)
  77de…  github.com/you/api  ~/dirigent/77de…
Last warnings and errors in its log
  2026-11-15T14:01:55Z  WARN connecting to control at …: …
```

- **Service**: the host's service, whichever installed it (`dirigent host install`, Homebrew,
  home-manager, NixOS's unit), as launchd or systemd tells it: running or not, its pid, the user it
  runs as, its last exit code, its file and log. A system daemon an earlier version installed shows
  too, with how to remove it.
- **Connection**: whether dirigent has the host connected, under what name.
- **Sessions running here**: its players (each is a `dirigent __player` process), with their
  sessions' repositories, and the sessions' directories with no player running.
- **Last warnings and errors** of its log (the file, or the journal).

`--json` gives the same for scripts.

### Nix modules

| Module | Runs as |
|---|---|
| `homeManagerModules.dirigent-host` | You: a launchd agent (macOS) or a systemd user service (Linux) |
| `nixosModules.dirigent-host` | A systemd system service, as `user` (required), from boot |

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
| `user` (NixOS) | string, required | the user it runs as: whose `dirigent login` it uses, whose `~/dirigent` its sessions go in |

Releases are built into the `dirigent.cachix.org` cache. Modules don't use the flake's own settings, so add it to your system's Nix (`nix.settings.extra-substituters = [ "https://dirigent.cachix.org" ];` and `extra-trusted-public-keys = [ "dirigent.cachix.org-1:9st/693o4bC8VbqMDHIoYcYn6Mp4PR/zLNCJ7YSI+GQ=" ];`), and use a release's tag (`github:adamchol/dirigent-0/v<version>`) without making its `nixpkgs` follow yours, or the CLI is built rather than downloaded.

Either way, sign in once with `dirigent login` as the host's user. The NixOS module's config is
`/etc/dirigent/host.toml`; its credentials are the user's own (`~/.config/dirigent/credentials.json`).

Earlier versions had `sudo dirigent host install --system` and a nix-darwin module, which ran the
host as a system daemon. On a Mac that daemon can't reach your login keychain or the Simulator, so
they're gone: remove the daemon (drop `services.dirigent-host` from your nix-darwin config, or
`sudo launchctl bootout system/dev.dirigent.host` and delete
`/Library/LaunchDaemons/dev.dirigent.host.plist`, `/etc/dirigent`), then install the agent as above.
`dirigent host status` points out a daemon left behind.

### By hand

`dirigent host run` runs the host in the foreground (`RUST_LOG=dirigent=debug` for more).

## Configuration

`~/.config/dirigent/host.toml` (the NixOS module's: `/etc/dirigent/host.toml`; `$DIRIGENT_HOST_CONFIG`
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
| `roots` | `~/dirigent`. A session's directory is reused in whichever root has it; new ones go in the first. |
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
