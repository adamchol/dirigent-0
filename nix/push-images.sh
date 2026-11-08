# The chart's images and the chart, to a registry (CI's, .github/workflows/images.yml, and yours):
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
images=(control chlive web migrations kube-tools rustfs ingest player-vm)
cd "${DNVR_ROOT:-$PWD}"

if [ -n "${REGISTRY_PASSWORD:-}" ]; then
  host=${registry%%/*}
  skopeo login "$host" -u "$REGISTRY_USER" --password-stdin <<<"$REGISTRY_PASSWORD"
  helm registry login "$host" -u "$REGISTRY_USER" --password-stdin <<<"$REGISTRY_PASSWORD"
fi

# all at once, so they build in parallel; each one's path after, from the store
nix build --no-link .#chart "${images[@]/%/-image}" --print-build-logs
for image in "${images[@]}"; do
  archive=$(nix build ".#$image-image" --no-link --print-out-paths)
  skopeo copy --retry-times 3 "docker-archive:$archive" "docker://$registry/dirigent-$image:$tag"
  for more in "$@"; do
    skopeo copy --retry-times 3 "docker://$registry/dirigent-$image:$tag" "docker://$registry/dirigent-$image:$more"
  done
done

chart=$(nix build .#chart --no-link --print-out-paths)
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
# the images' registry, and the ingestion subchart's image, which images.* don't reach
sed -i \
  -e "s|^  registry: .*|  registry: $registry|" \
  -e "s|^      repository: .*/dirigent-ingest$|      repository: $registry/dirigent-ingest|" \
  -e "/repository: .*\/dirigent-ingest$/{n;s|^      tag: .*|      tag: \"$tag\"|}" \
  "$out/dirigent/values.yaml"
helm package "$out/dirigent" -d "$out" --version "$version" --app-version "$tag"
helm push "$out/dirigent-$version.tgz" "oci://$registry/charts"
