# The chart's (chart/dirigent) own images, built from the flake with nix2container (chlive's, the
# ingestion's and Garage's are their projects'): `nix build .#<name>-image` gives the image's
# JSON, its layers each a derivation of its own, which `push-images` sends to the registry (by
# the layers it hasn't) and the test cluster loads (skopeo's `nix:` transport). What changes
# least is in a layer of its own (Bun, kopia, the tools, the players' VM's disk); what the images
# tell, `<version>+<rev>`, is in their configuration or the player's disk, so a new commit or
# release builds again only what it changed. The players' VM is two KubeVirt containerDisks: a
# NixOS disk image (nix/vm), and the player, apart, so that a new player doesn't make a new
# NixOS image.
{
  pkgs,
  lib,
  nixpkgs,
  nix2container,
  dirigent,
  web,
  openmig,
  # the stock players' VM's system (nix/vm), the flake's
  vmSystem,
  # the chart's appVersion, the images' tag
  version,
  # the commit they're built from
  rev,
}:
let
  inherit (nix2container) buildImage buildLayer;
  # what the images tell (control's and the web's /version, the player's `player_attached`)
  stamp = "${version}+${rev}";
  image =
    name: args:
    buildImage (
      {
        name = "dirigent-${name}";
        # the tag the cluster pulls (push-images pushes as others too); not in what's built
        tag = version;
        # the image's own layer (what `layers` hasn't) split by store paths, so what's ours is
        # apart from the libraries it links
        maxLayers = 8;
      }
      // args
    );
  # the files at /bin, /etc/ssl, … of the packages, and their closure
  env = name: paths: pkgs.buildEnv { inherit name paths; };
  # The disks hold store paths (the system's, the player's libraries) as bytes, which Nix would take
  # for references: an image would then carry their closure, which a containerDisk never reads
  disk =
    drv:
    lib.overrideDerivation drv (_: {
      __structuredAttrs = true;
      unsafeDiscardReferences.out = true;
    });
  playerDisk = disk (
    import "${nixpkgs}/nixos/lib/make-disk-image.nix" {
      inherit pkgs lib;
      inherit (vmSystem) config;
      format = "qcow2";
      partitionTableType = "legacy";
      diskSize = "auto";
      # room for what sessions add to the store; the qcow2 is sparse, so the image stays small
      additionalSpace = "24G";
      # where KubeVirt's containerDisk has it, /disk, so the layer is this output as it is (no copy
      # of the disk)
      baseName = "player";
      postVM = ''
        mkdir $out/disk
        mv $out/player.qcow2 $out/disk/
        rm -r $out/nix-support
      '';
    }
  );
in
{
  # with kopia, for the checkpoints of the sessions it deletes, and their repository's maintenance
  control = image "control" {
    layers = [
      (buildLayer {
        copyToRoot = [
          (env "dirigent-control-root" [
            pkgs.cacert
            pkgs.kopia
          ])
        ];
      })
    ];
    config = {
      Entrypoint = [ (lib.getExe dirigent.control) ];
      Env = [
        "PATH=/bin"
        "DIRIGENT_VERSION=${stamp}"
      ];
    };
  };

  # the UI's Bun server (web/src/server.ts): the app, and its proxies to control and chlive
  web = image "web" {
    layers = [ (buildLayer { deps = [ pkgs.bun ]; }) ];
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
        "DIRIGENT_VERSION=${stamp}"
      ];
      User = "65532";
    };
  };

  # the chart's Jobs and init containers: the schema migrations (openmig.toml and db/, in
  # /dirigent), the kopia repository and Garage's setup, chlive's config and its wait for
  # control's keys; and Nix, for the shared Nix store's generations
  tools =
    let
      files = pkgs.runCommand "dirigent-tools-files" { } ''
        mkdir -p $out/dirigent $out/tmp
        cp -r ${../db} $out/dirigent/db
        cp ${../openmig.toml} $out/dirigent/openmig.toml
      '';
    in
    image "tools" {
      layers = [
        (buildLayer {
          copyToRoot = [
            (env "dirigent-tools-root" (
              with pkgs;
              [
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
              ]
            ))
          ];
          maxLayers = 8;
        })
      ];
      copyToRoot = [ files ];
      perms = [
        {
          path = files;
          regex = "/tmp$";
          mode = "1777";
        }
      ];
      # cacert has only ca-bundle.crt, and Nix looks for ca-certificates.crt: without, a
      # generation's writer can't fetch from cache.nixos.org
      config.Env = [ "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt" ];
    };

  # KubeVirt's containerDisk: the disk image at /disk, readable by its qemu (uid 107)
  player-vm = image "player-vm" {
    layers = [ (buildLayer { copyToRoot = [ playerDisk ]; }) ];
  };

  # the player (nix/vm/base.nix mounts it by its serial, dirigent-player): its binary, alone on an
  # ISO 9660 disk, and its version (`version`, which the VM gives it)
  player = image "player" {
    copyToRoot = [
      (disk (
        pkgs.runCommand "dirigent-player-disk" { nativeBuildInputs = [ pkgs.xorriso ]; } ''
          mkdir -p disk $out/disk
          cp ${lib.getExe dirigent.player} disk/player
          echo -n ${stamp} > disk/version
          xorriso -as mkisofs -quiet -rational-rock -volid DIRIGENT_PLAYER -o $out/disk/player.iso disk
        ''
      ))
    ];
  };
}
