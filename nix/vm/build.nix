# `dirigent-build <dir>`: build the cluster's system (`player.nixos`) from the configuration in
# <dir>, sign it with DIRIGENT_NIX_SIGNING_KEY, put it in the cache (DIRIGENT_NIX_CACHE), and
# tell control (DIRIGENT_CONTROL_HTTP) how it went, as the build DIRIGENT_BUILD with
# DIRIGENT_BUILD_TOKEN: the system's store path, or the end of Nix's output. A VM in build mode
# runs it (base.nix); so does the dev stack's control, on its own machine.
{
  pkgs,
  nixpkgs,
}:
let
  # What builds the cluster's system from its configuration's files (`--arg configuration`):
  # nix/vm/system.nix's `checked`, on this image's nixpkgs.
  builder = pkgs.writeText "dirigent-system.nix" ''
    { configuration }:
    (import ${./.}/system.nix {
      nixpkgs = builtins.storePath "${nixpkgs}";
      system = "${pkgs.stdenv.hostPlatform.system}";
    }).checked configuration
  '';

in
pkgs.writeShellApplication {
  name = "dirigent-build";
  runtimeInputs = [
    pkgs.nix
    pkgs.curl
    pkgs.jq
    pkgs.coreutils
  ];
  text = ''
    dir=$1
    log=$(mktemp)
    key=$(mktemp)
    printf '%s' "''${DIRIGENT_NIX_SIGNING_KEY:-}" > "$key"
    report() {
      curl -fsS --retry 10 --retry-all-errors -X POST \
        -H "authorization: Bearer $DIRIGENT_BUILD_TOKEN" -H 'content-type: application/json' \
        --data-binary @- "$DIRIGENT_CONTROL_HTTP/system/builds/$DIRIGENT_BUILD"
    }
    failed() {
      cat "$log" >&2
      tail -c 65536 "$log" | jq -Rs '{ok: false, log: .}' | report
      exit 1
    }
    if [ -z "''${DIRIGENT_NIX_CACHE:-}" ] || [ -z "''${DIRIGENT_NIX_SIGNING_KEY:-}" ]; then
      echo "no cache (DIRIGENT_NIX_CACHE) or no key to sign with (DIRIGENT_NIX_SIGNING_KEY)" > "$log"
      failed
    fi
    echo "building the cluster's system"
    if ! out=$(nix build --no-link --print-out-paths -L --file ${builder} \
      --arg configuration "$dir/configuration.nix" 2> "$log"); then
      failed
    fi
    cat "$log" >&2
    echo "built $out; putting it in the cache"
    nix store sign --key-file "$key" --recursive "$out" 2> "$log" || failed
    nix copy --to "$DIRIGENT_NIX_CACHE" "$out" 2> "$log" || failed
    jq -n --arg path "$out" '{ok: true, store_path: $path}' | report
    echo "the cluster's system is $out"
  '';
}
