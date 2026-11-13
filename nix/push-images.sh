# The chart's own images and the chart, to a registry (CI's, .github/workflows/images.yml, and
# yours):
#   push-images [--registry R] TAG [TAG…]
# The images are built from the flake here, pushed as R/dirigent-<name>:TAG and tagged with the
# more TAGs too. The chart is pushed to oci://R/charts, its images R's at TAG: versioned TAG when
# that's a version (1.2.3), else its own version with -TAG (0.1.0-sha-abc1234). R is
# ghcr.io/adamchol by default; logged in already (`skopeo login`, `helm registry login`), or as
# $REGISTRY_USER with $REGISTRY_PASSWORD.

set -euo pipefail

registry=ghcr.io/adamchol
if [ "${1:-}" = --registry ]; then
  registry=$2
  shift 2
fi
[ $# -ge 1 ] || {
  echo "push-images [--registry R] TAG [TAG…]" >&2
  exit 2
}
tag=$1
shift
images=(control web tools player-vm player)
cd "${DNVR_ROOT:-$PWD}"

if [ -n "${REGISTRY_PASSWORD:-}" ]; then
  host=${registry%%/*}
  skopeo login "$host" -u "$REGISTRY_USER" --password-stdin <<<"$REGISTRY_PASSWORD"
  helm registry login "$host" -u "$REGISTRY_USER" --password-stdin <<<"$REGISTRY_PASSWORD"
fi

# built where $BUILD_STORE says (e.g. ssh-ng://eu.nixbuild.net, CI's), which keeps what the
# builds need, and only the archives and the chart copied here; else here (or your builders)
build() {
  if [ -n "${BUILD_STORE:-}" ]; then
    nix build --store "$BUILD_STORE" --eval-store auto --builders "" --max-jobs 2 --no-link "$@"
  else
    nix build --no-link "$@"
  fi
}
# here, from $BUILD_STORE: the path of $1, built
fetch() {
  local path
  path=$(build --print-out-paths "$1")
  [ -z "${BUILD_STORE:-}" ] || nix copy --from "$BUILD_STORE" --no-check-sigs "$path"
  echo "$path"
}

# all at once, so they build in parallel; each one's path after
installables=(.#chart)
for image in "${images[@]}"; do installables+=(".#$image-image"); done
build --print-build-logs "${installables[@]}"
for image in "${images[@]}"; do
  archive=$(fetch ".#$image-image")
  skopeo copy --retry-times 3 "docker-archive:$archive" "docker://$registry/dirigent-$image:$tag"
  for more in "$@"; do
    skopeo copy --retry-times 3 "docker://$registry/dirigent-$image:$tag" "docker://$registry/dirigent-$image:$more"
  done
done

chart=$(fetch .#chart)
semver='^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$'
if [[ $tag =~ $semver ]]; then
  version=$tag
else
  version="$(sed -n 's/^version: //p' "$chart/Chart.yaml")-$tag"
fi
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
cp -r "$chart" "$out/dirigent"
chmod -R u+w "$out/dirigent"
# the images' registry
sed -i "s|^  registry: .*|  registry: $registry|" "$out/dirigent/values.yaml"
helm package "$out/dirigent" -d "$out" --version "$version" --app-version "$tag"
helm push "$out/dirigent-$version.tgz" "oci://$registry/charts"
