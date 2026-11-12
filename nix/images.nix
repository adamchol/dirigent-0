# The chart's (chart/dirigent) own images, built from the flake (chlive's, the ingestion's and
# Garage's are their projects'): `nix build .#<name>-image` gives a docker-archive, which
# `push-images` sends to the registry (and the test cluster loads directly). The players'
# is a KubeVirt containerDisk: a NixOS disk image (nix/vm).
{
  pkgs,
  lib,
  nixpkgs,
  dirigent,
  web,
  openmig,
  version,
}:
let
  image =
    name: args:
    pkgs.dockerTools.buildLayeredImage (
      {
        name = "dirigent-${name}";
        tag = version;
      }
      // args
    );

  playerSystem =
    (import ./vm/system.nix {
      inherit nixpkgs dirigent;
      inherit (pkgs.stdenv.hostPlatform) system;
    }).eval
      ./vm/defaults.nix;
  playerDisk = import "${nixpkgs}/nixos/lib/make-disk-image.nix" {
    inherit pkgs lib;
    inherit (playerSystem) config;
    format = "qcow2";
    partitionTableType = "legacy";
    diskSize = "auto";
    # room for what sessions add to the store; the qcow2 is sparse, so the image stays small
    additionalSpace = "24G";
  };
in
{
  # with kopia, for the checkpoints of the sessions it deletes, and their repository's maintenance
  control = image "control" {
    contents = [
      pkgs.cacert
      pkgs.kopia
    ];
    config = {
      Entrypoint = [ (lib.getExe' dirigent "control") ];
      Env = [ "PATH=/bin" ];
    };
  };

  # the UI's Bun server (web/src/server.ts): the app, and its proxies to control and chlive
  web = image "web" {
    config = {
      # its bundle finds its files from the working directory
      WorkingDir = "${web}";
      Entrypoint = [
        (lib.getExe pkgs.bun)
        "server.js"
      ];
      Env = [
        "HOST=0.0.0.0"
        "PORT=8080"
      ];
      User = "65532";
    };
  };

  # the chart's Jobs and init containers: the schema migrations (openmig.toml and db/, in
  # /dirigent), the kopia repository and Garage's setup, chlive's config and its wait for
  # control's keys; and Nix, for the shared Nix store's generations
  tools = image "tools" {
    contents = with pkgs; [
      openmig
      kopia
      kubectl
      bashInteractive
      coreutils
      gnugrep
      curl
      jq
      cacert
      nix
    ];
    extraCommands = ''
      mkdir -p dirigent tmp
      chmod 1777 tmp
      cp -r ${../db} dirigent/db
      cp ${../openmig.toml} dirigent/openmig.toml
    '';
  };

  # KubeVirt's containerDisk: the disk image at /disk, readable by its qemu (uid 107)
  player-vm = pkgs.dockerTools.buildImage {
    name = "dirigent-player-vm";
    tag = version;
    copyToRoot = pkgs.runCommand "player-vm-disk" { } ''
      mkdir -p $out/disk
      cp ${playerDisk}/nixos.qcow2 $out/disk/player.qcow2
    '';
  };
}
