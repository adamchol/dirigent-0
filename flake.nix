{
  description = "dirigent: event-driven coding agents";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-parts.url = "github:hercules-ci/flake-parts";
    flake-parts.inputs.nixpkgs-lib.follows = "nixpkgs";
    # the rex runner is on this branch until it's merged
    dnvr.url = "github:dialohq/dnvr/feat/rex-runner";
    dnvr.inputs.nixpkgs.follows = "nixpkgs";
    # Kafka Connect's ClickHouse sink (exactly once) and its pause server, which chlive's
    # snapshots go through; the pause server's Keeper storage is on this branch
    continuous-backup = {
      url = "github:dialohq/clickhouse-continuous-backup/pause-server";
      flake = false;
    };
    # the chlive server (`chlive`) and its ClickHouse build
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
        in
        {
          formatter = pkgs.nixfmt;

          # The migrations match the schema: `openmig check` replays them into scratch servers.
          checks = lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
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
            # TODO: drop once chlive's package has them: its fileset misses the `wasm` workspace
            # member (cargo can't load the workspace), and reqwest's native-tls needs OpenSSL
            chlive = inputs'.chlive.packages.chlive.overrideAttrs (old: {
              src = inputs.chlive;
              buildInputs = old.buildInputs ++ [ pkgs.openssl ];
            });
          };
        };
    };
}
