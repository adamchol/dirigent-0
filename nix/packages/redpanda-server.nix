# Redpanda's server and rpk from the official Linux release tarball (nixpkgs no longer packages
# the server). It bundles its own glibc and ld.so, so it runs once the interpreter and wrapper
# scripts point into the store.
{
  stdenvNoCC,
  fetchurl,
  patchelf,
  binutils-unwrapped,
}:
let
  version = "25.2.1";
  assets = {
    x86_64-linux = {
      arch = "amd64";
      hash = "sha256-BqEEq5rImd0S+joU9HRJAEVWAufK0Vbb2s4O611EKzI=";
    };
    aarch64-linux = {
      arch = "arm64";
      hash = "sha256-fRIxeLrRkOET/Lj2l3qoeBrlmLaB/lwLSGScIowFhjc=";
    };
  };
  asset = assets.${stdenvNoCC.hostPlatform.system};
in
stdenvNoCC.mkDerivation {
  pname = "redpanda-server";
  inherit version;
  src = fetchurl {
    url = "https://vectorized-public.s3.us-west-2.amazonaws.com/releases/redpanda/${version}/redpanda-${version}-${asset.arch}.tar.gz";
    inherit (asset) hash;
  };
  sourceRoot = ".";
  nativeBuildInputs = [
    patchelf
    binutils-unwrapped
  ];
  dontStrip = true;
  # the bundled libraries must stay exactly as shipped
  dontPatchELF = true;
  installPhase = ''
    mkdir $out
    cp -r bin lib libexec $out
    chmod -R u+w $out
    for f in $out/libexec/*; do
      if [ "$(patchelf --print-interpreter "$f" 2>/dev/null)" = /opt/redpanda/lib/ld.so ]; then
        patchelf --set-interpreter $out/lib/ld.so "$f"
      fi
      # shipped with debug info (~3 GB)
      strip --strip-debug "$f" 2>/dev/null || true
    done
    substituteInPlace $out/bin/* --replace-quiet /opt/redpanda $out
  '';
}
