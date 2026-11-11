# dirigent's Rust binaries: `control`, `player` and the CLI, `dirigent`; the player tells its
# version, `<version>+<rev>`. Most tests need the running stack (`dnvr up`, then `e2e`): only
# the player's loading of the logs older players wrote (player/logs) runs here, so no image is
# built with a player that can't go on with old sessions.
{
  lib,
  rustPlatform,
  version,
  rev,
}:
rustPlatform.buildRustPackage {
  pname = "dirigent";
  inherit version;
  src = lib.fileset.toSource {
    root = ../..;
    fileset = lib.fileset.unions [
      ../../Cargo.toml
      ../../Cargo.lock
      ../../protocol
      ../../sync
      ../../player
      ../../cli
      ../../control
    ];
  };
  # chlive's client crates, at the commit Cargo.lock has
  cargoLock = {
    lockFile = ../../Cargo.lock;
    allowBuiltinFetchGit = true;
  };
  env.DIRIGENT_VERSION = "${version}+${rev}";
  cargoTestFlags = [
    "-p"
    "dirigent-player"
  ];
  checkFlags = [ "old_logs" ];
  meta.mainProgram = "control";
}
