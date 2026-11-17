# The web UI (web/), built: its Bun server with the app bundled in (web/build.ts), run as
# `bun server.js`; its version is its environment's (DIRIGENT_VERSION, the image's), so a commit
# that doesn't change web/ doesn't build it again. chlive's client (chliveClient: its src/ and
# its WebAssembly core) is copied inside the app, where web/tsconfig.json has it, so its own
# imports find the app's node_modules.
{
  lib,
  stdenvNoCC,
  bun,
  chliveClient,
}:
let
  # the dependencies, fetched from what names them alone: update the hash when bun.lock changes
  nodeModules = stdenvNoCC.mkDerivation {
    pname = "dirigent-web-node-modules";
    version = "0.1.0";
    src = lib.fileset.toSource {
      root = ../../web;
      fileset = lib.fileset.unions [
        ../../web/package.json
        ../../web/bun.lock
        ../../web/bunfig.toml
      ];
    };
    nativeBuildInputs = [ bun ];
    dontConfigure = true;
    dontFixup = true;
    buildPhase = ''
      export HOME=$TMPDIR
      bun install --frozen-lockfile --ignore-scripts --no-progress
    '';
    installPhase = "cp -r node_modules $out";
    outputHashMode = "recursive";
    outputHashAlgo = "sha256";
    outputHash = "sha256-AcPSQvyF4DkFsHE0vNk+MiBHSsa10DiI7NaKl8r4qUY=";
  };
in
stdenvNoCC.mkDerivation {
  pname = "dirigent-web";
  version = "0.1.0";
  src = lib.fileset.toSource {
    root = ../../web;
    fileset = lib.fileset.unions [
      ../../web/package.json
      ../../web/bun.lock
      ../../web/bunfig.toml
      ../../web/tsconfig.json
      ../../web/build.ts
      ../../web/wasm-url.ts
      ../../web/src
    ];
  };
  nativeBuildInputs = [ bun ];
  buildPhase = ''
    export HOME=$TMPDIR
    cp -r ${nodeModules} node_modules
    chmod -R u+w node_modules
    patchShebangs node_modules
    cp -r --no-preserve=mode ${chliveClient} chlive-client
    bun build.ts
  '';
  installPhase = "cp -r dist $out";
  passthru = { inherit nodeModules; };
}
