# Machines and systems

How the cluster runs players: their VMs and how big they are, VMs booted ahead of time, the NixOS
system they run (yours, if you give one), and a Nix store they can share.

## Players' VMs

Each session's player runs in a KubeVirt `VirtualMachineInstance` of its own, named
`player-<session>-<epoch>`, made by control from the chart's template. It has:

- **The NixOS disk** (`dirigent-player-vm` image): NixOS with common tools on the agent's `PATH`
  (coreutils and friends, archives, curl, wget, ssh, ripgrep, fd, jq, git, gh, gcc, make,
  pkg-config, Python), Nix with `nixpkgs` pinned to the image's, and about 24 GB free for the Nix
  store.
- **The player's disk** (`dirigent-player` image): the player binary, apart from the NixOS disk, so
  a release that changes only dirigent's code pulls a few MB, not a new VM image.
- **The workspace disk**: an empty disk of the session's size, formatted at boot: the workspace and
  the player's state. It goes with the VM; the [checkpoint](../guides/workspaces.md#checkpoints)
  brings it back.
- **Its environment**: the session, its player id and epoch, and a lease token, as cloud-init data;
  the kopia token and S3 key as small disks. API keys and secrets come later, on the lease.

The VM runs as root, with network access (see
[networkPolicy](configuration.md#networkpolicy)). It powers off when its player exits, and control
deletes it. When Kubernetes deletes it (a node drained), the player gets
`player.stopGrace - player.checkpointMargin` (55 minutes by default) to finish its turn, then
checkpoints and hands over to a new VM.

Useful labels: `dirigent.dev/session`, `dirigent.dev/epoch`, `dirigent.dev/warm`,
`dirigent.dev/cpu`, `.../memory`, `.../disk`.

```sh
kubectl -n dirigent get vmi -L dirigent.dev/session,dirigent.dev/cpu,dirigent.dev/memory
```

## Machines

A session's **machine** is its VM's cores, memory and workspace disk.

- **The default machine** is `player.cpu`, `player.memory` and `player.workspaceSize` (2 cores, 4
  GiB, 20 GiB).
- **A session can ask for its own** when it's made (the web's **Machine** section, a dirigent's
  `create_session`), and keeps it. A session that didn't ask gets the default machine *as it is
  when it starts*, so changing the default applies to them at their next start.
- **Limits** (`player.limits`) cap what a session may ask for; by default there are none.
- **Per-user defaults** (`player.defaultsPerUser: true`): each user sets their own default machine
  in Settings; what they leave out is the chart's.

Machines are KubeVirt's `domain.cpu.cores`, `domain.memory.guest`, and the work disk's size. Make
sure your nodes fit the largest machine you allow.

## Warm VMs

Booting a VM takes a minute or so. `player.warm.count` keeps that many VMs booted and waiting, with
their player idle; a starting session on the same machine takes one and skips the boot, and control
boots another to replace it.

- Without per-user defaults: `count` VMs on the default machine.
- With `player.defaultsPerUser`: `count` VMs for each user who made a session within
  `player.warm.activeWithin` seconds (7 days), on that user's default machine.
- A session asking for any other machine gets a VM of its own (and waits for it).
- Each warm VM costs its machine all the time.
- Warm VMs are replaced when they no longer match: on an upgrade, a new default machine, a new
  players' system, or a new shared-store generation.

Warm VMs are `warm-<player>`, labelled `dirigent.dev/warm`.

## The cluster's own system

By default the VMs run the image's stock NixOS ([`nix/vm/defaults.nix`](../../nix/vm/defaults.nix)).
`player.nixos.files` gives them a system of your own: more packages, services, settings, anything
NixOS does.

```yaml
player:
  nixos:
    files:
      configuration.nix: |
        { pkgs, dirigent, ... }: {
          imports = [ dirigent.defaults ./node.nix ];   # keep the stock tools
          environment.systemPackages = [ pkgs.go pkgs.postgresql ];
          services.postgresql.enable = true;
        }
      node.nix: |
        { pkgs, ... }: { environment.systemPackages = [ pkgs.nodejs ]; }
```

`configuration.nix` is the entry point; other files are imported by relative path.
`imports = [ dirigent.defaults ];` keeps the stock packages; leave it out to replace them.

How it works:

1. **Build.** When control starts, it builds the system in a VM (`build-<id>`, sized by
   `player.nixos.build`), signs it, and puts it in the Nix cache (`nix.cache`, in your S3). It
   builds once per configuration and VM image: a release that changes only dirigent's code doesn't
   rebuild it.
2. **Boot.** Each session's VM boots the stock system, fetches yours from the cache, and switches
   into it with kexec, kernel and all. The disks stay as they are.
3. **Fallbacks.** While it builds, or if the build fails, sessions run the stock system. If a
   session's VM doesn't come up with it, the session is started again on the stock system; if
   that works, the system is marked **broken** and the stock system runs until the configuration
   changes.

What dirigent needs is checked when it's built (the player's service, the disks' mounts, the cache
in `nix.conf`); a configuration that takes it away fails to build. Everything else is yours,
including breaking the VM. Every session can read the built system: keep secrets out of it.

Where to see how it went:

- `kubectl -n dirigent describe configmap dirigent-player-nixos`: Events **Building**, **Built**
  (with the store path), **BuildFailed** (with the build's log), **Broken**.
- The web's Settings, and `GET /api/system`: `building`, `ready`, `failed` or `broken`.
- control's log.

## The shared Nix store

Sessions fetch and build Nix paths as they work (`nix shell`, `nix develop`, builds). With
`nix.shared.enabled`, what VMs add is collected, now and then, into a **generation** of a store
every VM mounts under its own, so later VMs have it without fetching it again.

Requirements:

- KubeVirt's `EnableVirtioFsStorageVolumes` feature gate (the store is mounted over virtiofs);
- Kubernetes 1.36, for a `MutatingAdmissionPolicy` that keeps a deleted VM's virtiofs mounted
  through its grace period;
- a `ReadWriteMany` volume across nodes (`nix.shared.storageClass`, `size`); `ReadWriteOnce` works
  only on a single-node cluster.

How it works:

- Each VM mounts the newest generation read-only, as the lower layer of an overlay over its
  `/nix/store` (Nix's experimental local-overlay store).
- Players report the paths their VM added. At most every `nix.shared.every` seconds, when there's
  something new, control runs a Job (`dirigent-nix-gen-<n>`) that writes the next generation.
- `nix.shared.keep` generations are kept, besides those VMs use.

What goes in (`nix.shared.promote`):

- **`upstream`** (default): only paths cache.nixos.org signed. Those are the same for everyone, so
  sharing them leaks nothing.
- **`all`**: everything, sessions' own builds included. Anything a session put in its store (a
  private repository's source a flake copied there, a token a build wrote) becomes readable by every
  later session, and a session can put what it likes in paths later sessions use. Only for
  clusters whose users trust each other.

The volume (`dirigent-nix-store`) is kept when the chart is uninstalled.
