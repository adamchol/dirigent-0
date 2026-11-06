# The images the chart (chart/dirigent) runs, built from the flake: `nix build .#<name>-image`
# gives a docker-archive, which `push-images` sends to the registry (and the test cluster loads
# directly). The players' is a KubeVirt containerDisk: a NixOS disk image (nix/player-vm.nix).
{
  pkgs,
  lib,
  nixpkgs,
  dirigent,
  web,
  chlive,
  openmig,
  continuous-backup,
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

  playerSystem = nixpkgs.lib.nixosSystem {
    inherit (pkgs.stdenv.hostPlatform) system;
    modules = [ (import ./player-vm.nix { inherit dirigent; }) ];
  };
  playerDisk = import "${nixpkgs}/nixos/lib/make-disk-image.nix" {
    inherit pkgs lib;
    inherit (playerSystem) config;
    format = "qcow2";
    partitionTableType = "legacy";
    diskSize = "auto";
    additionalSpace = "1G";
  };
in
{
  control = image "control" {
    contents = [ pkgs.cacert ];
    config.Entrypoint = [ (lib.getExe' dirigent "control") ];
  };

  chlive = image "chlive" {
    contents = [ pkgs.cacert ];
    config.Entrypoint = [ (lib.getExe' chlive "chlive") ];
  };

  # the static UI, served by nginx with the chart's config (it proxies /api and /live)
  web = image "web" {
    contents = [
      pkgs.nginx
      pkgs.fakeNss
    ];
    extraCommands = ''
      mkdir -p srv tmp var/cache/nginx var/log/nginx
      chmod 1777 tmp var/cache/nginx var/log/nginx
      ln -s ${web} srv/web
    '';
    config.Entrypoint = [
      (lib.getExe pkgs.nginx)
      "-g"
      "daemon off;"
    ];
  };

  # the schema migrations (openmig.toml, db/), run by the chart's hook Jobs
  migrations = image "migrations" {
    contents = [
      openmig
      pkgs.bashInteractive
      pkgs.coreutils
      pkgs.curl
      pkgs.cacert
    ];
    extraCommands = ''
      mkdir -p dirigent tmp
      chmod 1777 tmp
      cp -r ${../db} dirigent/db
      cp ${../openmig.toml} dirigent/openmig.toml
    '';
    config.WorkingDir = "/dirigent";
  };

  # continuous-backup's ingestion (Kafka Connect with the ClickHouse sink, and the pause server),
  # as its own image, which isn't published yet: the chart's ingestion.connect.image
  ingest =
    let
      backup = (pkgs.callPackage "${continuous-backup}/nix/backup.nix" { }).overrideAttrs (_: {
        doCheck = false;
      });
      connector = pkgs.callPackage "${continuous-backup}/nix/clickhouse-kafka-connect.nix" { };
    in
    image "ingest" {
      contents = pkgs.buildEnv {
        name = "connect-runtime";
        paths = with pkgs; [
          apacheKafka
          jre
          bash
          coreutils
          curl
          jq
          dockerTools.caCertificates
          backup
          connector
        ];
        pathsToLink = [
          "/bin"
          "/lib"
          "/plugins"
          "/etc"
        ];
      };
      extraCommands = "mkdir -p tmp && chmod 1777 tmp";
      config = {
        Cmd = [
          "${pkgs.apacheKafka}/bin/connect-distributed.sh"
          "/etc/durable-clickhouse/connect-distributed.properties"
        ];
        User = "65532:65532";
        Env = [
          "SSL_CERT_FILE=/etc/ssl/certs/ca-bundle.crt"
          # Kafka Connect's default heap is 2 GiB
          "KAFKA_HEAP_OPTS=-Xms128M -Xmx512M"
        ];
      };
    };

  # the chart's RustFS (checkpoints.rustfs.enabled), instead of your S3
  rustfs = image "rustfs" {
    contents = [ pkgs.rustfs ];
    config.Entrypoint = [ (lib.getExe pkgs.rustfs) ];
  };

  # the chart's Jobs that make the kopia repository and Secrets
  kube-tools = image "kube-tools" {
    contents = with pkgs; [
      kopia
      kubectl
      bashInteractive
      coreutils
      gnugrep
      curl
      jq
      cacert
    ];
    extraCommands = "mkdir -p tmp && chmod 1777 tmp";
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
