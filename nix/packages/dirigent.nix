# dirigent's Rust binaries: `control`, `player` and `chat`. The tests need the running stack
# (`dnvr up`, then `e2e`), so none run here.
{
  lib,
  rustPlatform,
}:
rustPlatform.buildRustPackage {
  pname = "dirigent";
  version = "0.1.0";
  src = lib.fileset.toSource {
    root = ../..;
    fileset = lib.fileset.unions [
      ../../Cargo.toml
      ../../Cargo.lock
      ../../protocol
      ../../player
      ../../chat
      ../../control
    ];
  };
  # chlive's client crates, at the commit Cargo.lock has
  cargoLock = {
    lockFile = ../../Cargo.lock;
    allowBuiltinFetchGit = true;
  };
  doCheck = false;
  meta.mainProgram = "control";
}
