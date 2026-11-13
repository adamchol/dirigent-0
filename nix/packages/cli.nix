# dirigent's CLI alone, `dirigent` (cli/, built with the rest: dirigent.nix), with git on its
# PATH for `dirigent sync`.
{
  lib,
  runCommand,
  makeWrapper,
  git,
  dirigent,
}:
runCommand "dirigent-cli-${dirigent.cli.version}"
  {
    nativeBuildInputs = [ makeWrapper ];
    meta.mainProgram = "dirigent";
  }
  ''
    makeWrapper ${lib.getExe dirigent.cli} $out/bin/dirigent --prefix PATH : ${lib.makeBinPath [ git ]}
  ''
