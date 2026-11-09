# The chart on a test cluster here: k3s (its state in .dnvr/k3s; pods 10.42/16, services
# 10.43/16), KubeVirt and CloudNativePG, stand-ins for ClickHouse and Kafka (stand-ins.yaml), the
# images loaded straight into k3s, and the chart with values.yaml (mock players).
#   kube-test up        start k3s (in the background; its log in .dnvr/k3s/k3s.log)
#   kube-test install   install KubeVirt, CloudNativePG and the stand-ins, load the images, and
#                       install (or upgrade) the chart
#   kube-test load      build and load the images again
#   kube-test e2e       the cluster's test (player/src/session/chlive.rs, `kube_e2e`), through
#                       forwarded ports to control and chlive
#   kube-test down      stop k3s (its pods and VMs stop with it; the state stays)
#   kube-test kubectl … kubectl on the test cluster
#   kube-test helm …    helm on it
# Needs root (k3s), /dev/kvm, and about 4 GB of memory: stop `dnvr up` first.

set -euo pipefail

root="${DNVR_ROOT:?run in the dev shell}"
dir="$root/.dnvr/k3s"
export KUBECONFIG="$dir/kubeconfig"
kubevirt=v1.9.1
cnpg=1.30.1

ctr() { k3s ctr --address /run/k3s/containerd/containerd.sock -n k8s.io "$@"; }

up() {
  mkdir -p "$dir"
  if [ -e "$dir/k3s.pid" ] && kill -0 "$(cat "$dir/k3s.pid")" 2>/dev/null; then
    echo "k3s is running"
  else
    setsid k3s server --data-dir "$dir/data" --write-kubeconfig "$KUBECONFIG" \
      --write-kubeconfig-mode 600 --disable traefik --disable metrics-server \
      --node-name dirigent-test --cluster-cidr 10.42.0.0/16 --service-cidr 10.43.0.0/16 \
      `# a dev machine's disk: images loaded here can't be pulled again, so they're kept` \
      --kubelet-arg=image-gc-high-threshold=98 --kubelet-arg=image-gc-low-threshold=97 \
      --kubelet-arg='eviction-hard=nodefs.available<2%,imagefs.available<2%' \
      >"$dir/k3s.log" 2>&1 </dev/null &
    echo $! >"$dir/k3s.pid"
  fi
  timeout 300 bash -c 'until kubectl get nodes 2>/dev/null | grep -q " Ready"; do sleep 3; done'
  kubectl get nodes
}

load() {
  for image in control web tools player-vm; do
    archive=$(nix build "$root#$image-image" --no-link --print-out-paths)
    ctr images import "$archive" >/dev/null
    echo "[kube-test] loaded $image"
  done
}

install() {
  local m="$dir/manifests"
  mkdir -p "$m"
  [ -s "$m/kubevirt-operator.yaml" ] || curl -fsSL -m 120 -o "$m/kubevirt-operator.yaml" \
    "https://github.com/kubevirt/kubevirt/releases/download/$kubevirt/kubevirt-operator.yaml"
  [ -s "$m/kubevirt-cr.yaml" ] || curl -fsSL -m 120 -o "$m/kubevirt-cr.yaml" \
    "https://github.com/kubevirt/kubevirt/releases/download/$kubevirt/kubevirt-cr.yaml"
  [ -s "$m/cnpg.yaml" ] || curl -fsSL -m 120 -o "$m/cnpg.yaml" \
    "https://github.com/cloudnative-pg/cloudnative-pg/releases/download/v$cnpg/cnpg-$cnpg.yaml"
  kubectl apply -f "$m/kubevirt-operator.yaml" >/dev/null
  kubectl apply -f "$m/kubevirt-cr.yaml" >/dev/null
  kubectl apply --server-side -f "$m/cnpg.yaml" >/dev/null
  kubectl -n kubevirt wait kubevirt/kubevirt --for=condition=Available --timeout=600s
  kubectl -n cnpg-system wait deploy/cnpg-controller-manager --for=condition=Available --timeout=300s
  kubectl create namespace dirigent --dry-run=client -o yaml | kubectl apply -f - >/dev/null
  kubectl -n dirigent apply -f "$root/nix/kube-test/stand-ins.yaml" >/dev/null
  kubectl -n dirigent wait deploy/clickhouse deploy/redpanda --for=condition=Available --timeout=300s
  load
  chart=$(nix build "$root#chart" --no-link --print-out-paths)
  helm upgrade --install dirigent "$chart" -n dirigent -f "$root/nix/kube-test/values.yaml" \
    --timeout 20m --wait --wait-for-jobs
  kubectl -n dirigent get pods
}

e2e() {
  local logs="$dir/port-forward.log"
  kubectl -n dirigent port-forward svc/dirigent-control 18090:8090 >"$logs" 2>&1 &
  local control=$!
  kubectl -n dirigent port-forward svc/dirigent-chlive 18080:8080 >>"$logs" 2>&1 &
  local chlive=$!
  # shellcheck disable=SC2064 # the pids now: the locals are gone when it runs; either may have
  # ended already
  trap "kill $control $chlive 2>/dev/null || true" EXIT
  timeout 30 bash -c 'until curl -fs -o /dev/null http://127.0.0.1:18090/auth; do sleep 1; done'
  cd "$root" || exit
  DIRIGENT_KUBE_E2E=1 CHLIVE_URL=ws://127.0.0.1:18080/live DIRIGENT_CONTROL_HTTP=http://127.0.0.1:18090 \
    timeout 1200 cargo test -p dirigent-player kube_e2e -- --nocapture "$@"
}

down() {
  if [ -e "$dir/k3s.pid" ]; then
    kill "$(cat "$dir/k3s.pid")" 2>/dev/null || true
    rm -f "$dir/k3s.pid"
  fi
  # k3s leaves its containers running; these stop them
  "$(dirname "$(readlink -f "$(command -v k3s)")")/k3s-killall.sh" >/dev/null 2>&1 || true
  echo "k3s stopped"
}

case "${1:-}" in
  up) up ;;
  install) install ;;
  load) load ;;
  e2e) shift; e2e "$@" ;;
  down) down ;;
  kubectl) shift; exec kubectl "$@" ;;
  helm) shift; exec helm "$@" ;;
  *) sed -n '1,/^$/p' "$0" >&2; exit 2 ;;
esac
