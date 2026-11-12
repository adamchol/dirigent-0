# What every players' VM has, the stock one and a cluster's own (`player.nixos`, nix/vm/system.nix
# checks a configuration kept it): NixOS on KubeVirt running one player, which it powers off
# after. What the VM is given, by the guest contract:
# - its environment, as the NoCloud user data (an env file: the template's lines, then
#   control's for the player: DIRIGENT_SESSION, DIRIGENT_PLAYER_ID, DIRIGENT_EPOCH; or a warm
#   VM's, DIRIGENT_WARM and DIRIGENT_PLAYER_ID, its player told its session later);
# - Secrets as disks with serials `dirigent-secret<n>`, a file per key, named as the variable
#   (DIRIGENT_KOPIA_TOKEN; API keys and sessions' secrets come on the lease instead);
# - an empty disk with serial `dirigent-work`, formatted on first boot: the workspace and the
#   player's state. Its files of any size are checkpointed, those over the log's 50 MB there alone.
# Deleted (a node drained, the VM deleted), KubeVirt shuts the VM down and the player gets
# SIGTERM; the VM's grace period (`terminationGracePeriodSeconds`) bounds how long it has to
# finish the turn, checkpoint and hand the session over (its DIRIGENT_STOP_GRACE is less).
{
  dirigent,
  nixpkgs,
}:
{
  lib,
  pkgs,
  modulesPath,
  ...
}:
let
  config = "/mnt/dirigent/config";
  secrets = "/run/dirigent/secrets";
  work = "/var/lib/dirigent";
  # the cluster's configuration's files, in a VM building it (a ConfigMap's disk)
  nixos = "/run/dirigent/nixos";

  # The VM's environment, for a unit's script: the user data, then each secret disk's files as
  # variables (mounted once).
  environment = ''
    set -a
    . ${config}/user-data
    for disk in /dev/disk/by-id/virtio-dirigent-secret*; do
      [ -e "$disk" ] || continue
      dir=${secrets}/$(basename "$disk")
      mkdir -p "$dir"
      mountpoint -q "$dir" || mount -o ro "$disk" "$dir"
      for f in "$dir"/*; do
        [ -f "$f" ] && export "$(basename "$f")=$(cat "$f")"
      done
    done
    set +a
  '';

  # what builds the cluster's system, in a VM control made for it
  build = import ./build.nix { inherit pkgs nixpkgs dirigent; };
in
{
  imports = [
    "${modulesPath}/profiles/qemu-guest.nix"
    "${modulesPath}/profiles/minimal.nix"
  ];

  system.stateVersion = "25.11";
  networking.hostName = "player";
  networking.useDHCP = true;
  # KubeVirt's console log (`kubectl logs <virt-launcher> -c guest-console-log`): the boot,
  # then the player's output
  boot.kernelParams = [ "console=ttyS0" ];
  services.journald.settings.Journal.ForwardToConsole = true;
  boot.loader.grub.device = "/dev/vda";
  boot.growPartition = true;
  fileSystems."/" = {
    device = "/dev/disk/by-label/nixos";
    fsType = "ext4";
    autoResize = true;
  };
  fileSystems.${config} = {
    device = "/dev/disk/by-label/cidata";
    fsType = "iso9660";
    options = [ "ro" ];
  };
  fileSystems.${work} = {
    device = "/dev/disk/by-id/virtio-dirigent-work";
    fsType = "ext4";
    autoFormat = true;
  };

  # `nixpkgs#…` and `<nixpkgs>` are the image's own nixpkgs, not whatever is latest
  nix.registry.nixpkgs.to = {
    type = "path";
    path = "${nixpkgs}";
  };
  nix.nixPath = [ "nixpkgs=${nixpkgs}" ];
  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];
  # nor fetched from the global registry
  nix.settings.flake-registry = "";
  # the cluster's cache and shared store (dirigent-nix), when it has them; nix/vm/system.nix
  # checks it's kept
  nix.extraOptions = "!include /run/dirigent/nix.conf";

  # What Nix has of the cluster's:
  # - the cache (DIRIGENT_NIX_CACHE, an s3:// store with `profile=dirigent`), where what control
  #   builds is (the cluster's system), signed with control's key (DIRIGENT_NIX_KEY), read with
  #   the S3 key on a secret disk (accessKey, secretKey);
  # - the shared store (the chart's `nix.shared`), when the initrd put its generation under the
  #   VM's own (dirigent-store): Nix's store is the two, an experimental local-overlay store,
  #   whose lower is never changed nor collected.
  systemd.services.dirigent-nix = {
    description = "dirigent's cache and shared store, for Nix";
    wantedBy = [ "multi-user.target" ];
    before = [
      "nix-daemon.service"
      "dirigent-system.service"
      "player.service"
    ];
    unitConfig.RequiresMountsFor = [ config ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
    };
    path = [
      pkgs.util-linux
      pkgs.systemd
      pkgs.jq
    ];
    script = ''
      ${environment}
      umask 077
      mkdir -p /root/.aws /run/dirigent
      : > /run/dirigent/nix.conf
      if [ -n "''${DIRIGENT_NIX_CACHE:-}" ]; then
        printf '[dirigent]\naws_access_key_id = %s\naws_secret_access_key = %s\n' \
          "''${accessKey:-}" "''${secretKey:-}" > /root/.aws/credentials
        printf 'extra-substituters = %s\nextra-trusted-public-keys = %s\n' \
          "$DIRIGENT_NIX_CACHE" "''${DIRIGENT_NIX_KEY:-}" >> /run/dirigent/nix.conf
      fi
      gen=''${DIRIGENT_NIX_GENERATION:-}
      if [ -n "$gen" ] && grep -q ' /nix/store overlay ' /proc/mounts; then
        lower=$(jq -rn --arg g "$gen" '"local?root=/nix/.shared/gen-\($g)&read-only=true" | @uri')
        {
          echo "extra-experimental-features = local-overlay-store read-only-local-store"
          echo "store = local-overlay://?lower-store=$lower&upper-layer=/nix/.upper/store&check-mount=true"
          echo "ignore-gc-delete-failure = true"
        } >> /run/dirigent/nix.conf
      fi
      chmod 644 /run/dirigent/nix.conf
      systemctl try-restart nix-daemon.service
    '';
  };

  # The shared store's generation (DIRIGENT_NIX_GENERATION, on the virtiofs `dirigent-nix`) under
  # the VM's own, as /nix/store's overlay: its lower, the generation; its upper, the image's
  # store, moved aside to /nix/.upper/store on the first boot (its paths noted, as the player
  # tells only what the VM added). It's mounted before the switch to the system, by the paths
  # it has there, which Nix checks the mount by. Without either, the store is the image's alone.
  boot.initrd.availableKernelModules = [
    "virtiofs"
    "overlay"
    "iso9660"
  ];
  boot.loader.grub.copyKernels = true;
  boot.initrd.systemd.initrdBin = [
    pkgs.coreutils
    pkgs.gnused
    pkgs.util-linux
  ];
  boot.initrd.systemd.services.dirigent-store = {
    description = "the shared Nix store, under the VM's own";
    wantedBy = [ "initrd-fs.target" ];
    before = [
      "initrd-fs.target"
      "sysroot-nix-store.mount"
    ];
    requires = [ "sysroot.mount" ];
    after = [ "sysroot.mount" ];
    unitConfig.DefaultDependencies = false;
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
    };
    script = ''
      mkdir -p /run/cidata
      # its link, a moment after the root disk's
      for _ in $(seq 50); do
        [ -e /dev/disk/by-label/cidata ] && break
        sleep 0.1
      done
      mount -o ro /dev/disk/by-label/cidata /run/cidata || exit 0
      gen=$(sed -n 's/^DIRIGENT_NIX_GENERATION=//p' /run/cidata/user-data | tail -n 1)
      umount /run/cidata
      [ -n "$gen" ] || exit 0
      mkdir -p /sysroot/nix/.shared
      if ! mount -t virtiofs -o ro dirigent-nix /sysroot/nix/.shared; then
        echo "no shared store (virtiofs dirigent-nix): the VM's own alone" >&2
        exit 0
      fi
      if [ ! -d "/sysroot/nix/.shared/gen-$gen/nix/store" ]; then
        echo "the shared store has no generation $gen: the VM's own alone" >&2
        umount /sysroot/nix/.shared
        exit 0
      fi
      if [ ! -d /sysroot/nix/.upper/store ]; then
        mkdir -p /sysroot/nix/.upper/work
        ls /sysroot/nix/store | sed 's|^|/nix/store/|' > /sysroot/nix/.upper/image-paths
        mv /sysroot/nix/store /sysroot/nix/.upper/store
        mkdir /sysroot/nix/store
      fi
      # by the system's paths: the initrd's own /nix, those of its root
      mkdir -p /nix/.shared /nix/.upper
      mount --bind /sysroot/nix/.shared /nix/.shared
      mount --bind /sysroot/nix/.upper /nix/.upper
      mount -t overlay overlay -o "lowerdir=/nix/.shared/gen-$gen/nix/store,upperdir=/nix/.upper/store,workdir=/nix/.upper/work" /sysroot/nix/store
      umount /nix/.shared /nix/.upper
    '';
  };

  # The cluster's own system (DIRIGENT_SYSTEM, built by control from `player.nixos`), when this
  # isn't it yet: fetched, then booted into, kernel and all, with kexec (the disks stay as they
  # are). Not fetched or not booted, the stock system runs the session.
  systemd.services.dirigent-system = {
    description = "the cluster's own system";
    wantedBy = [ "multi-user.target" ];
    before = [ "player.service" ];
    after = [
      "dirigent-nix.service"
      "network-online.target"
    ];
    wants = [ "network-online.target" ];
    unitConfig.RequiresMountsFor = [ config ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
    };
    path = [
      pkgs.nix
      pkgs.kexec-tools
      pkgs.util-linux
      pkgs.systemd
    ];
    script = ''
      ${environment}
      want=''${DIRIGENT_SYSTEM:-}
      if [ -z "$want" ] || [ "$(readlink -f /run/current-system)" = "$want" ]; then
        exit 0
      fi
      if ! nix-store --realise "$want" >/dev/null; then
        echo "the cluster's system $want couldn't be fetched: the stock one runs" >&2
        exit 0
      fi
      if ! kexec --load "$want/kernel" --initrd="$want/initrd" \
        --append="init=$want/init $(cat "$want/kernel-params")" || ! systemctl kexec; then
        echo "the cluster's system $want couldn't be booted: the stock one runs" >&2
        exit 0
      fi
      # the VM goes on in that system, so the player isn't started in this one
      exec sleep infinity
    '';
  };

  systemd.services.player = {
    description = "dirigent's player";
    wantedBy = [ "multi-user.target" ];
    after = [ "network-online.target" ];
    wants = [ "network-online.target" ];
    unitConfig = {
      RequiresMountsFor = [
        config
        work
      ];
      # done (the session stopped, failed, or handed over), the VM is too
      SuccessAction = "poweroff";
      FailureAction = "poweroff";
    };
    # what the system has first (its tools, as the configuration has them), then the player's own
    path = [
      "/run/current-system/sw"
      pkgs.git
      pkgs.util-linux
      pkgs.kopia
      pkgs.bash
      pkgs.coreutils
      pkgs.ripgrep
      pkgs.nix
    ];
    environment = {
      HOME = "/root";
      KOPIA_CHECK_FOR_UPDATES = "false";
    };
    serviceConfig = {
      Type = "exec";
      # SIGTERM to the player alone; whatever is left, killed once the VM's grace is up
      KillMode = "mixed";
      TimeoutStopSec = "infinity";
    };
    # or, in a VM control made to build the cluster's system (DIRIGENT_MODE=build), the build
    script = ''
      ${environment}
      if [ "''${DIRIGENT_MODE:-}" = build ]; then
        mkdir -p ${nixos}
        mountpoint -q ${nixos} || mount -o ro /dev/disk/by-id/virtio-dirigent-nixos ${nixos}
        exec ${build}/bin/dirigent-build ${nixos}
      fi
      exec ${dirigent}/bin/player --workdir ${work}/workspace --state-dir ${work}/state
    '';
  };
}
