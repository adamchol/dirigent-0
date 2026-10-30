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
      url = "git+https://github.com/dialohq/chlive?ref=all-in";
      inputs.nixpkgs.follows = "nixpkgs";
      inputs.flake-parts.follows = "flake-parts";
      inputs.dnvr.follows = "dnvr";
      inputs.continuous-backup.follows = "continuous-backup";
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
          presets,
          dnvrState,
          inputs',
          ...
        }:
        {
          formatter = pkgs.nixfmt;

          dnvr.shells.default = import ./nix/shell.nix {
            inherit pkgs presets dnvrState;
            inherit (inputs) continuous-backup;
            inherit (inputs'.chlive.packages) clickhouse chlive;
          };
        };
    };
}
