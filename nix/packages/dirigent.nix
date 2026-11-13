# dirigent's Rust binaries, with crane: the workspace's dependencies once (`deps`, which no
# commit of ours changes), then each binary on them: `control`, `player` and the CLI,
# `dirigent`. The player tells its version, `<version>+<rev>`; control knows the players' VM
# image it goes with, `vm` (what a cluster's own system is built for). Most tests need the
# running stack (`dnvr up`, then `e2e`): only the player's loading of the logs older players
# wrote (player/logs) runs here, so no image is built with a player that can't go on with old
# sessions.
{
  lib,
  craneLib,
  symlinkJoin,
  version,
  rev,
  vm,
}:
let
  common = {
    pname = "dirigent";
    inherit version;
    src = lib.fileset.toSource {
      root = ../..;
      # the cluster tests (player/tests) build no binary: editing them rebuilds nothing
      fileset = lib.fileset.difference (lib.fileset.unions [
        ../../Cargo.toml
        ../../Cargo.lock
        ../../protocol
        ../../sync
        ../../player
        ../../cli
        ../../control
      ]) ../../player/tests;
    };
    strictDeps = true;
  };
  deps = craneLib.buildDepsOnly common;
  binary =
    crate: args:
    craneLib.buildPackage (
      common
      // {
        pname = crate;
        cargoArtifacts = deps;
        cargoExtraArgs = "--locked -p ${crate}";
        doCheck = false;
        env = {
          DIRIGENT_VERSION = "${version}+${rev}";
          DIRIGENT_VM = vm;
        };
      }
      // args
    );
  control = binary "dirigent-control" { meta.mainProgram = "control"; };
  player = binary "dirigent-player" {
    doCheck = true;
    cargoTestExtraArgs = "old_logs";
    meta.mainProgram = "player";
  };
  cli = binary "dirigent-cli" { meta.mainProgram = "dirigent"; };
in
symlinkJoin {
  name = "dirigent-${version}";
  paths = [
    control
    player
    cli
  ];
  passthru = {
    inherit
      deps
      control
      player
      cli
      ;
  };
  meta.mainProgram = "control";
}
