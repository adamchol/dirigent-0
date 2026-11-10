{
  description = "dirigent: event-driven coding agents";

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
    };
    # schema migrations for ClickHouse and Postgres (openmig.toml, db/)
    openmig = {
      url = "github:adamchol/openmig";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-parts.follows = "flake-parts";
      inputs.dnvr.follows = "dnvr";
    };
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
          # TODO: drop once chlive's package has them: its fileset misses the `wasm` workspace
          # member (cargo can't load the workspace), and reqwest's native-tls needs OpenSSL
          chlive = inputs'.chlive.packages.chlive.overrideAttrs (old: {
            src = inputs.chlive;
            buildInputs = old.buildInputs ++ [ pkgs.openssl ];
          });
          dirigent = pkgs.callPackage ./nix/packages/dirigent.nix { };
          # chlive's TS client, as the web UI takes it (web/chlive-client): its src/, and wasm/, its
          # WebAssembly core
          chliveClient = pkgs.runCommand "chlive-client" { } ''
            mkdir $out
            cp -r ${inputs.chlive}/client/src $out/src
            # TODO: drop the override once chlive pins it: wasm-bindgen's CLI must be its
            # Cargo.lock's crate
            cp -r ${
              inputs'.chlive.packages.chlive-wasm.override { wasm-bindgen-cli = pkgs.wasm-bindgen-cli_0_2_127; }
            } $out/wasm
          '';
          web = pkgs.callPackage ./nix/packages/web.nix { inherit chliveClient; };
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
              ;
            inherit (inputs) nixpkgs;
            version = "0.1.0";
          };
        in
        {
          formatter = pkgs.nixfmt;

          packages = {
            inherit dirigent web chart;
            default = dirigent;
            # the dev shell's `push-images`, without the shell (CI's)
            push-images = pkgs.writeShellApplication {
              name = "push-images";
              runtimeInputs = [
                pkgs.skopeo
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
            inherit chlive chliveClient;
          };
        };
    };
}
