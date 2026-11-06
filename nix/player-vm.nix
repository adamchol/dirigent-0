# The players' VM (KubeVirt; the chart's template, control's `--kube-template`): NixOS running
# one player, which it powers off after. What the VM is given, by the guest contract:
# - its environment, as the NoCloud user data (an env file: the template's lines, then
#   control's for the player: DIRIGENT_SESSION, DIRIGENT_PLAYER_ID, DIRIGENT_EPOCH);
# - Secrets as disks with serials `dirigent-secret<n>`, a file per key, named as the variable
#   (DIRIGENT_KOPIA_TOKEN, ANTHROPIC_API_KEY, …);
# - an empty disk with serial `dirigent-work`, formatted on first boot: the workspace and the
#   player's state.
# Deleted (a node drained, the VM deleted), KubeVirt shuts the VM down and the player gets
# SIGTERM; the VM's grace period (`terminationGracePeriodSeconds`) bounds how long it has to
# finish the turn, checkpoint and hand the session over (its DIRIGENT_STOP_GRACE is less).
{
  dirigent,
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

  # what a coder's commands find, besides the player's own
  environment.systemPackages = with pkgs; [
    git
    ripgrep
    curl
    jq
  ];
  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];

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
    path = [
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
    script = ''
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
      exec ${lib.getExe' dirigent "player"} --workdir ${work}/workspace --state-dir ${work}/state
    '';
  };
}
