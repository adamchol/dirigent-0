# The web UI (web/), built: static files for any web server (the chart's nginx). chlive's client
# comes from its flake input, copied inside the app so its own imports find the app's
# node_modules. Its WebAssembly core (client/wasm, built in chlive's shell, not a package of its
# flake) is a stub that fails to load: the client then only doesn't show inserts before chlive
# has them.
{
  lib,
  stdenvNoCC,
  bun,
  chliveSrc,
}:
let
  src = lib.fileset.toSource {
    root = ../../web;
    fileset = lib.fileset.unions [
      ../../web/package.json
      ../../web/bun.lock
      ../../web/index.html
      ../../web/tsconfig.json
      ../../web/vite.config.ts
      ../../web/src
    ];
  };
  # the dependencies, fetched: update the hash when bun.lock changes
  nodeModules = stdenvNoCC.mkDerivation {
    pname = "dirigent-web-node-modules";
    version = "0.1.0";
    inherit src;
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
    outputHash = "sha256-udPbPi5sxMsMzS8CsoCVAIDTbvrAfPtyEmPikudtEBM=";
  };
in
stdenvNoCC.mkDerivation {
  pname = "dirigent-web";
  version = "0.1.0";
  inherit src;
  nativeBuildInputs = [ bun ];
  buildPhase = ''
    export HOME=$TMPDIR
    cp -r ${nodeModules} node_modules
    chmod -R u+w node_modules
    patchShebangs node_modules
    mkdir chlive-client
    cp -r ${chliveSrc}/client/src chlive-client/src
    mkdir chlive-client/wasm
    cat > chlive-client/wasm/chlive_wasm.js <<'EOF'
    export default async function init() {
      throw new Error("chlive-core isn't in this build");
    }
    EOF
    CHLIVE_CLIENT=$PWD/chlive-client bun node_modules/vite/bin/vite.js build
  '';
  installPhase = "cp -r dist $out";
}
