{
  description = "dirigent: event-driven coding agents";

  # releases' CLI (.github/workflows/cachix.yml) and the dev shell (e2e.yml), built by CI; Nix asks
  # before using it, unless it's trusted (--accept-flake-config, or the cache in nix.conf)
  nixConfig = {
    extra-substituters = [ "https://dirigent.cachix.org" ];
    extra-trusted-public-keys = [
      "dirigent.cachix.org-1:9st/693o4bC8VbqMDHIoYcYn6Mp4PR/zLNCJ7YSI+GQ="
    ];
  };

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs";
    # the rex runner and the Garage preset are on this branch until they're merged
    dnvr.url = "github:dialohq/dnvr/feat/garage-preset";
    dnvr.inputs.nixpkgs.follows = "nixpkgs";
    # Kafka Connect's ClickHouse sink (exactly once) and its pause server, which chlive's
    # snapshots go through: its chart (vendored into ours) and the dev stack's; its image is
    # published (ingestion.connect.image)
    continuous-backup = {
      url = "github:dialohq/clickhouse-continuous-backup/main";
      flake = false;
    };
    # the chlive server: its TS client (the web's), the dev stack's server and ClickHouse; its
    # image is published as sha-<12 chars of the commit>, the chart's chlive.image (checked)
    chlive = {
      url = "git+https://github.com/dialohq/chlive?ref=master";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-parts.follows = "flake-parts";
      inputs.dnvr.follows = "dnvr";
      inputs.continuous-backup.follows = "continuous-backup";
      inputs.nix2container.follows = "nix2container";
      inputs.crane.follows = "crane";
    };
    # schema migrations for ClickHouse and Postgres (openmig.toml, db/)
    openmig = {
      url = "github:adamchol/openmig";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-parts.follows = "flake-parts";
      inputs.dnvr.follows = "dnvr";
    };
    # the Rust builds: the workspace's dependencies once, then each binary on them
    crane.url = "github:ipetkov/crane";
    # the images (nix/images.nix): each layer built once, the same whatever the version, and
    # pushed by the layers the registry hasn't
    nix2container.url = "github:nlewo/nix2container";
    nix2container.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      imports = [ inputs.dnvr.flakeModule ];

      # `dirigent host` as a service, always a user's: home-manager's (macOS and Linux), or NixOS's
      # system unit running as a user
      flake =
        let
          self = inputs.self;
        in
        {
          homeManagerModules.dirigent-host = import ./nix/modules/home-manager.nix { inherit self; };
          nixosModules.dirigent-host = import ./nix/modules/nixos.nix { inherit self; };
        };

      perSystem =
        {
          pkgs,
          lib,
          presets,
          dnvrState,
          inputs',
          ...
        }:
        let
          inherit (inputs'.chlive.packages) clickhouse;
          inherit (inputs'.openmig.packages) openmig;
          postgresql = pkgs.postgresql_17;
          # chlive's own, as its flake builds it: what a cache of its builds has
          inherit (inputs'.chlive.packages) chlive;
          # the stock players' VM's system (nix/vm), which the VM image has (images.nix); a
          # cluster's own system is built for it, so control knows it (its store path's hash)
          vmSystem =
            (import ./nix/vm/system.nix {
              inherit (inputs) nixpkgs;
              inherit (pkgs.stdenv.hostPlatform) system;
            }).eval
              ./nix/vm/defaults.nix;
          # the commit dirigent's built from, which its parts tell with their version
          rev = inputs.self.shortRev or inputs.self.dirtyShortRev or "dirty";
          vm =
            if pkgs.stdenv.hostPlatform.isLinux then
              lib.substring 11 32 (
                builtins.unsafeDiscardStringContext vmSystem.config.system.build.toplevel.outPath
              )
            else
              "none";
          # the CLI tells its version (`<appVersion>+<rev>`): the release it follows, and the commit
          # it was built from; control and the player are told theirs by their images
          dirigent = pkgs.callPackage ./nix/packages/dirigent.nix {
            craneLib = inputs.crane.mkLib pkgs;
            version = chartVersion "appVersion";
            inherit rev vm;
          };
          # the CLI alone, for its users (`nix run github:adamchol/dirigent-0#cli -- login …`)
          cli = pkgs.callPackage ./nix/packages/cli.nix { inherit dirigent; };
          # chlive's TS client, as the web UI takes it (web/chlive-client): its src/, and wasm/, its
          # WebAssembly core
          chliveClient = pkgs.runCommand "chlive-client" { } ''
            mkdir $out
            cp -r ${inputs.chlive}/client/src $out/src
            cp -r ${inputs'.chlive.packages.chlive-wasm} $out/wasm
          '';
          # its version is its image's (DIRIGENT_VERSION)
          web = pkgs.callPackage ./nix/packages/web.nix { inherit chliveClient; };
          # the chart's `version` or `appVersion` (Chart.yaml), which a release's tag is (v<it>);
          # the images' tag here is its appVersion, which the chart pulls when images.tag is empty
          chartVersion =
            key:
            lib.head (
              builtins.match ".*\n${key}: ([^\n]+)\n.*" (builtins.readFile ./chart/dirigent/Chart.yaml)
            );
          # the chart, with continuous-backup's vendored in (its dependency)
          chart = pkgs.runCommand "dirigent-chart" { } ''
            cp -r ${./chart/dirigent} $out
            chmod -R u+w $out
            mkdir -p $out/charts
            cp -r ${inputs.continuous-backup}/chart $out/charts/durable-clickhouse-sink
          '';
          images = import ./nix/images.nix {
            inherit
              pkgs
              lib
              dirigent
              web
              openmig
              vmSystem
              rev
              ;
            inherit (inputs) nixpkgs;
            inherit (inputs'.nix2container.packages) nix2container;
            version = chartVersion "appVersion";
          };
          # skopeo with nix2container's `nix:` transport, which copies its images
          skopeo = inputs'.nix2container.packages.skopeo-nix2container;
        in
        {
          formatter = pkgs.nixfmt;

          packages = {
            inherit
              dirigent
              cli
              web
              chart
              ;
            default = dirigent;
            # what builds the cluster's own players' system (nix/vm/build.nix), for the dev
            # stack's control, which runs it here
            player-build = import ./nix/vm/build.nix {
              inherit pkgs;
              inherit (inputs) nixpkgs;
            };
            # the dev shell's `push-images`, without the shell (CI's)
            push-images = pkgs.writeShellApplication {
              name = "push-images";
              runtimeInputs = [
                skopeo
                pkgs.kubernetes-helm
              ];
              text = ''exec bash ${./nix/push-images.sh} "$@"'';
            };
          }
          // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux (
            lib.mapAttrs' (name: lib.nameValuePair "${name}-image") images
          );

          # The migrations match the schema: `openmig check` replays them into scratch servers.
          # The chart's chlive.image is the flake's chlive's. The chart renders no Secrets
          # (control makes them), and the same each time, so a rendering can be kept in git.
          checks = lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
            # one version, the chart's and its images' (a release's tag is checked against it)
            chart-version =
              assert lib.assertMsg (
                chartVersion "version" == chartVersion "appVersion"
              ) "chart/dirigent/Chart.yaml: version and appVersion differ";
              pkgs.runCommand "dirigent-chart-version-check" { } "touch $out";
            chart-render =
              pkgs.runCommand "dirigent-chart-render-check" { nativeBuildInputs = [ pkgs.kubernetes-helm ]; }
                ''
                  export HOME=$TMPDIR
                  for i in 1 2; do
                    helm template dirigent ${chart} -n dirigent -a postgresql.cnpg.io/v1 -a kubevirt.io/v1 \
                      -f ${./nix/kube-test/values.yaml} > render$i.yaml
                  done
                  ! grep -n '^kind: Secret$' render1.yaml \
                    || { echo "the chart renders a Secret (control makes them: control ensure-secrets)" >&2; exit 1; }
                  diff render1.yaml render2.yaml \
                    || { echo "the chart renders differently each time" >&2; exit 1; }
                  touch $out
                '';
            # the stock players' VM, booted (KVM)
            player-vm = import ./nix/vm/test.nix {
              inherit pkgs;
              inherit (inputs) nixpkgs;
            };
            # the player runs on its own disk, on what the VM image has: all it needs but itself
            player-runs-on-the-image =
              pkgs.runCommand "dirigent-player-closure-check"
                {
                  player = pkgs.closureInfo { rootPaths = [ dirigent.player ]; };
                  image = pkgs.closureInfo { rootPaths = [ vmSystem.config.system.build.toplevel ]; };
                }
                ''
                  missing=$(grep -vxF -f $image/store-paths $player/store-paths | grep -vxF  || true)
                  [ -z "$missing" ] || { echo "the VM image hasn't what the player needs: $missing" >&2; exit 1; }
                  touch $out
                '';
            chlive-image =
              let
                tag = "sha-${lib.substring 0 12 inputs.chlive.rev}";
              in
              pkgs.runCommand "dirigent-chlive-image-check" { } ''
                grep -q 'repository: ghcr.io/dialohq/chlive, tag: ${tag},' ${./chart/dirigent/values.yaml} \
                  || { echo "chart/dirigent/values.yaml: chlive.image.tag isn't ${tag}, flake.lock's chlive" >&2; exit 1; }
                touch $out
              '';
            migrations =
              pkgs.runCommand "dirigent-migrations-check"
                {
                  nativeBuildInputs = [
                    openmig
                    postgresql
                    clickhouse
                  ];
                  src = lib.fileset.toSource {
                    root = ./.;
                    fileset = lib.fileset.unions [
                      ./openmig.toml
                      ./db
                    ];
                  };
                }
                ''
                  export HOME=$TMPDIR XDG_CACHE_HOME=$TMPDIR/cache
                  cd $src
                  openmig check --all
                  touch $out
                '';
          };

          dnvr.shells.default = import ./nix/shell.nix {
            inherit
              pkgs
              presets
              dnvrState
              clickhouse
              openmig
              postgresql
              ;
            inherit (inputs) continuous-backup;
            inherit chlive chliveClient skopeo;
          };
        };
    };
}
