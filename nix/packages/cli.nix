# dirigent's CLI alone, `dirigent` (cli/), with git on its PATH for `dirigent sync`. The whole
# workspace is its source, as cargo needs every member; only the CLI is built.
{
  lib,
  rustPlatform,
  makeWrapper,
  git,
}:
rustPlatform.buildRustPackage {
  pname = "dirigent-cli";
  inherit ((lib.importTOML ../../cli/Cargo.toml).package) version;
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
  cargoBuildFlags = [
    "-p"
    "dirigent-cli"
  ];
  nativeBuildInputs = [ makeWrapper ];
  postInstall = ''
    wrapProgram $out/bin/dirigent --prefix PATH : ${lib.makeBinPath [ git ]}
  '';
  doCheck = false;
  meta.mainProgram = "dirigent";
}
