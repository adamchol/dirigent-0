# dirigent's Rust binaries, with crane: the workspace's dependencies once (`deps`, which no
# commit or release of ours changes: only Cargo.lock does), then each binary on them: `control`,
# `player` and the CLI, `dirigent`. Only the CLI is built with its version, `<version>+<rev>`
# (control and the player are told theirs by their images, nix/images.nix, so a release or a
# commit that doesn't change them doesn't build them again); control knows the players' VM image
# it goes with, `vm` (what a cluster's own system is built for). Most tests need the running
# stack (`dnvr up`, then `e2e`): only the player's loading of the logs older players wrote
# (player/logs) runs here, so no image is built with a player that can't go on with old
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
    # the crates' (all one), not the release's
    version = (lib.importTOML ../../player/Cargo.toml).package.version;
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
      }
      // args
    );
  control = binary "dirigent-control" {
    env.DIRIGENT_VM = vm;
    meta.mainProgram = "control";
  };
  player = binary "dirigent-player" {
    doCheck = true;
    cargoTestExtraArgs = "old_logs";
    meta.mainProgram = "player";
  };
  cli = binary "dirigent-cli" {
    version = "${version}+${rev}";
    env.DIRIGENT_VERSION = "${version}+${rev}";
    meta.mainProgram = "dirigent";
  };
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
