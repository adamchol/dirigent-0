# The chart on a test cluster here: k3s (its state in .dnvr/k3s; pods 10.42/16, services
# 10.43/16), KubeVirt and CloudNativePG, stand-ins for ClickHouse and Kafka (stand-ins.yaml), the
# images loaded straight into k3s, and the chart with values.yaml (mock players).
#   kube-test up        start k3s (in the background; its log in .dnvr/k3s/k3s.log); with the
#                       chart installed, restart what doesn't come back by itself
#   kube-test install [helm args…]
#                       install KubeVirt (with virtiofs for volumes, for the shared Nix store),
#                       CloudNativePG and the stand-ins, load the images, and install (or
#                       upgrade) the chart, with values.yaml and the args (e.g. `-f more.yaml`)
#   kube-test load      build and load the images again; control and the web restart, and the
#                       warm VMs go (the tags stay the same)
#   kube-test e2e [--scenario default|system|all] [test name…]
#                       the cluster's tests (player/tests/kube.rs), each scenario's chart in
#                       turn (default: values.yaml; system: and system.yaml), each test on its
#                       own and on past failures; everything to .dnvr/k3s/runs/<time>/<scenario>:
#                       summary.txt, tests/<test>.log, sessions/<id>.tsv (their events),
#                       vms/<vmi>.log (serial consoles, kept after the VMs go), logs/, events.txt
#   kube-test gc        delete what the Nix store has but the loaded images, the dev shell and
#                       the workspace's built dependencies, and all but the last 10 runs
#   kube-test remote <command…>
#                       the same on the test instance (EC2, tagged project=dirigent-test):
#                       started if stopped, this tree copied there, its runs copied back;
#                       `kube-test remote stop` stops it (it stops itself after an idle hour)
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
images=(control web tools player-vm player)
# where the kubelet keeps pods' volumes: the VMs' serial consoles are there
kubelet=/var/lib/kubelet

ctr() { k3s ctr --address /run/k3s/containerd/containerd.sock -n k8s.io "$@"; }
say() { echo "[kube-test] $*"; }
installed() { helm -n dirigent status dirigent >/dev/null 2>&1; }

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
  if installed; then
    heal
  fi
}

# Connect's and the pause server's connections to Kafka and Keeper, and chlive's to them, don't
# come back by themselves when those restart (k3s did, or the stand-ins changed): they're
# restarted.
heal() {
  kubectl -n dirigent wait deploy/clickhouse deploy/redpanda --for=condition=Available --timeout=300s \
    >/dev/null || true
  for d in dirigent-ingest-connect dirigent-ingest-pause-server dirigent-chlive; do
    kubectl -n dirigent rollout restart "deploy/$d" >/dev/null
  done
  for d in dirigent-ingest-connect dirigent-ingest-pause-server dirigent-chlive dirigent-control; do
    kubectl -n dirigent rollout status "deploy/$d" --timeout=300s >/dev/null || true
  done
  say "the ingestion and chlive restarted"
}

load() {
  mkdir -p "$dir/roots"
  for image in "${images[@]}"; do
    # kept from `kube-test gc` (and the Nix store's own)
    archive=$(nix build "$root#$image-image" --out-link "$dir/roots/$image" --print-out-paths)
    ctr images import "$archive" >/dev/null
    say "loaded $image"
  done
  if installed; then
    kubectl -n dirigent rollout restart deploy/dirigent-control deploy/dirigent-web >/dev/null
    kubectl -n dirigent rollout status deploy/dirigent-control --timeout=300s >/dev/null
    kubectl -n dirigent delete vmi -l dirigent.dev/warm --wait=false >/dev/null
    say "control and the web restarted, the warm VMs replaced"
  fi
}

chart() {
  mkdir -p "$dir/roots"
  nix build "$root#chart" --out-link "$dir/roots/chart" --print-out-paths
}

install() {
  local m="$dir/manifests"
  mkdir -p "$m" "$dir/roots"
  [ -s "$m/kubevirt-operator.yaml" ] || curl -fsSL -m 120 -o "$m/kubevirt-operator.yaml" \
    "https://github.com/kubevirt/kubevirt/releases/download/$kubevirt/kubevirt-operator.yaml"
  [ -s "$m/kubevirt-cr.yaml" ] || curl -fsSL -m 120 -o "$m/kubevirt-cr.yaml" \
    "https://github.com/kubevirt/kubevirt/releases/download/$kubevirt/kubevirt-cr.yaml"
  [ -s "$m/cnpg.yaml" ] || curl -fsSL -m 120 -o "$m/cnpg.yaml" \
    "https://github.com/cloudnative-pg/cloudnative-pg/releases/download/v$cnpg/cnpg-$cnpg.yaml"
  kubectl apply -f "$m/kubevirt-operator.yaml" >/dev/null
  # its webhook checks the KubeVirt resource, and isn't served until the operator is up (after
  # k3s starts again too)
  kubectl -n kubevirt wait deploy/virt-operator --for=condition=Available --timeout=300s >/dev/null
  timeout 120 bash -c "until kubectl apply -f '$m/kubevirt-cr.yaml' >/dev/null; do sleep 5; done"
  kubectl apply --server-side -f "$m/cnpg.yaml" >/dev/null
  kubectl -n kubevirt wait kubevirt/kubevirt --for=condition=Available --timeout=600s
  # one of each of KubeVirt's pods on the one node: the second ones' requests leave too little
  # memory for a VM of 2 GiB
  kubectl -n kubevirt patch kubevirt kubevirt --type merge -p \
    '{"spec":{"infra":{"replicas":1},"configuration":{"developerConfiguration":{"featureGates":["EnableVirtioFsStorageVolumes"]}}}}' \
    >/dev/null
  kubectl -n kubevirt scale deploy/virt-operator --replicas=1 >/dev/null
  kubectl -n cnpg-system wait deploy/cnpg-controller-manager --for=condition=Available --timeout=300s
  kubectl create namespace dirigent --dry-run=client -o yaml | kubectl apply -f - >/dev/null
  kubectl -n dirigent apply -f "$root/nix/kube-test/stand-ins.yaml" >/dev/null
  kubectl -n dirigent wait deploy/clickhouse deploy/redpanda --for=condition=Available --timeout=300s
  # dev-container mode makes any topic a client asks for, and Connect's own topics, made that way
  # before Connect makes them, aren't compacted: Connect won't start
  kubectl -n dirigent exec deploy/redpanda -- rpk cluster config set auto_create_topics_enabled false \
    >/dev/null
  load
  local was=false
  installed && was=true
  helm upgrade --install dirigent "$(chart)" -n dirigent -f "$root/nix/kube-test/values.yaml" \
    --timeout 20m --wait --wait-for-jobs "$@"
  [ "$was" = false ] || heal
  kubectl -n dirigent get pods
}

# The VMs' serial consoles, copied from the launcher pods' volumes every 2 s into $1 (each
# <vmi>.log), and the shared store's writers' logs into $2, until killed.
collect() {
  local vms=$1 writers=$2 uid vmi pod log
  mkdir -p "$vms" "$writers"
  while true; do
    kubectl -n dirigent get pods -l kubevirt.io=virt-launcher \
      -o jsonpath='{range .items[*]}{.metadata.uid} {.metadata.labels.vm\.kubevirt\.io/name}{"\n"}{end}' \
      2>/dev/null | while read -r uid vmi; do
      for log in "$kubelet/pods/$uid/volumes/kubernetes.io~empty-dir/private"/*/virt-serial0-log; do
        [ -f "$log" ] && cp "$log" "$vms/$vmi.log" 2>/dev/null
      done
    done || true
    for pod in $(kubectl -n dirigent get pods -l app.kubernetes.io/component=nix-writer -o name 2>/dev/null || true); do
      kubectl -n dirigent logs "$pod" >"$writers/${pod#pod/}.log" 2>/dev/null || true
    done
    sleep 2
  done
}

# One scenario's run into $2: the chart with its values, each test on its own, then what
# happened.
scenario() {
  local name=$1 out=$2
  shift 2
  local values=(-f "$root/nix/kube-test/values.yaml")
  [ "$name" = system ] && values+=(-f "$root/nix/kube-test/system.yaml")
  mkdir -p "$out/tests" "$out/sessions" "$out/logs"
  local start=$SECONDS
  say "$name: installing"
  if ! helm upgrade --install dirigent "$(chart)" -n dirigent "${values[@]}" \
    --timeout 20m --wait --wait-for-jobs >"$out/helm.log" 2>&1; then
    echo "$name: the chart didn't install (helm.log)" >>"$out/summary.txt"
    return 1
  fi
  echo "installed in $((SECONDS - start))s" >>"$out/summary.txt"
  # Connect, restarted, gives its connector to a worker only once the one before is given up for
  # gone (scheduled.rebalance.max.delay.ms, 5 minutes); until then chlive can't read the logs
  start=$SECONDS
  if timeout 420 bash -c 'until kubectl -n dirigent exec deploy/dirigent-ingest-connect -c connect -- \
    curl -fs localhost:8083/connectors/dirigent-ingest-session-events/status 2>/dev/null |
    grep -q "\"tasks\":\[{\"id\":0,\"state\":\"RUNNING\""; do sleep 5; done'; then
    echo "ingesting in $((SECONDS - start))s" >>"$out/summary.txt"
  else
    echo "the session_events connector isn't running (logs/dirigent-ingest-connect.log)" >>"$out/summary.txt"
  fi

  collect "$out/vms" "$out/writers" &
  local collector=$!
  kubectl -n dirigent port-forward svc/dirigent-control 18090:8090 >"$out/port-forward.log" 2>&1 &
  local control=$!
  kubectl -n dirigent port-forward svc/dirigent-chlive 18080:8080 >>"$out/port-forward.log" 2>&1 &
  local chlive=$!
  timeout 60 bash -c 'until curl -fs -o /dev/null http://127.0.0.1:18090/auth; do sleep 1; done'

  local tests failed=0 t status took
  tests=$(cd "$root" && cargo test -q -p dirigent-player --test kube -- --list 2>/dev/null |
    sed -n 's/: test$//p')
  [ $# -gt 0 ] && tests=$(printf '%s\n' "$tests" | grep -Fx -f <(printf '%s\n' "$@") || true)
  for t in $tests; do
    say "$name: $t"
    start=$SECONDS
    status=ok
    (cd "$root" && DIRIGENT_KUBE_E2E=1 CHLIVE_URL=ws://127.0.0.1:18080/live \
      DIRIGENT_CONTROL_HTTP=http://127.0.0.1:18090 \
      timeout 1800 cargo test -q -p dirigent-player --test kube -- --exact "$t" --nocapture) \
      >"$out/tests/$t.log" 2>&1 || { status=FAILED; failed=1; }
    took=$((SECONDS - start))
    printf '%-7s %5ss  %s  %s\n' "$status" "$took" "$t" \
      "$(grep -o 'session [0-9a-f-]*' "$out/tests/$t.log" | cut -d' ' -f2 | tr '\n' ' ')" \
      >>"$out/summary.txt"
  done

  kill "$collector" "$control" "$chlive" 2>/dev/null || true
  wait "$collector" "$control" "$chlive" 2>/dev/null || true
  gather "$out"
  [ "$failed" = 0 ]
}

# What happened: the pods' logs, the events, the VMs' and pods' state, control's answers, and
# the test sessions' events (from ClickHouse, a few seconds after, for the sink).
gather() {
  local out=$1 d s
  for d in dirigent-control dirigent-chlive dirigent-web dirigent-ingest-connect \
    dirigent-ingest-pause-server dirigent-ingest-recovery; do
    kubectl -n dirigent logs "deploy/$d" --all-containers --prefix >"$out/logs/$d.log" 2>&1 || true
  done
  kubectl get events -A --sort-by=.lastTimestamp >"$out/events.txt" 2>&1 || true
  kubectl -n dirigent get vmi,pods,jobs -o wide >"$out/objects.txt" 2>&1 || true
  kubectl -n dirigent get vmi -o yaml >"$out/vmis.yaml" 2>&1 || true
  sleep 5
  for s in $(grep -ho 'session [0-9a-f-]*' "$out"/tests/*.log 2>/dev/null | cut -d' ' -f2 | sort -u); do
    kubectl -n dirigent exec deploy/clickhouse -- clickhouse-client -q "
      SELECT ts, kind, if(kind = 'files_changed', '(bundle)', data)
      FROM dirigent.session_events WHERE session_id = '$s' ORDER BY offset FORMAT TSV" \
      >"$out/sessions/$s.tsv" 2>&1 || true
  done
}

e2e() {
  local which=all
  if [ "${1:-}" = --scenario ]; then
    which=$2
    shift 2
  fi
  local scenarios
  case "$which" in
    all) scenarios=(default system) ;;
    default | system) scenarios=("$which") ;;
    *) echo "no scenario $which" >&2; exit 2 ;;
  esac
  local run failed=0 s
  run="$dir/runs/$(date -u +%Y%m%dT%H%M%SZ)"
  mkdir -p "$run"
  for s in "${scenarios[@]}"; do
    scenario "$s" "$run/$s" "$@" || failed=1
    { echo "== $s"; cat "$run/$s/summary.txt"; } >>"$run/summary.txt"
  done
  cat "$run/summary.txt"
  say "everything is in $run"
  [ "$failed" = 0 ]
}

gc() {
  mkdir -p "$dir/roots"
  # what the next build starts from, kept too: the dev shell, and the workspace's dependencies
  nix develop "$root" --profile "$dir/roots/shell" -c true >/dev/null 2>&1
  nix build "$root#dirigent.deps" --out-link "$dir/roots/deps"
  nix-collect-garbage >/dev/null
  if [ -d "$dir/runs" ]; then
    ls -1d "$dir/runs"/*/ 2>/dev/null | head -n -10 | xargs -r rm -rf
  fi
  df -h "$root" | tail -1
}

# The test instance: its id and state, by its tag.
instance() {
  aws ec2 describe-instances --filters Name=tag:project,Values=dirigent-test \
    Name=instance-state-name,Values=pending,running,stopping,stopped \
    --query 'Reservations[0].Instances[0].[InstanceId,State.Name,SecurityGroups[0].GroupId]' --output text
}

remote() {
  export AWS_PROFILE="${AWS_PROFILE:-dirigent-test}"
  local key="${DIRIGENT_TEST_SSH_KEY:-$HOME/.ssh/dirigent-test}"
  local id state group ip me
  read -r id state group < <(instance)
  [ "$id" != None ] || { echo "no instance tagged project=dirigent-test" >&2; exit 1; }
  if [ "${1:-}" = stop ]; then
    aws ec2 stop-instances --instance-ids "$id" >/dev/null
    say "$id stopping"
    return
  fi
  if [ "$state" != running ]; then
    [ "$state" = stopping ] && aws ec2 wait instance-stopped --instance-ids "$id"
    aws ec2 start-instances --instance-ids "$id" >/dev/null
    aws ec2 wait instance-running --instance-ids "$id"
    say "$id started"
  fi
  ip=$(aws ec2 describe-instances --instance-ids "$id" \
    --query 'Reservations[0].Instances[0].PublicIpAddress' --output text)
  # SSH from here only, wherever here is now
  me=$(curl -fsS -m 10 https://checkip.amazonaws.com)
  if ! aws ec2 describe-security-groups --group-ids "$group" \
    --query 'SecurityGroups[0].IpPermissions[].IpRanges[].CidrIp' --output text | grep -qw "$me/32"; then
    aws ec2 revoke-security-group-ingress --group-id "$group" --ip-permissions "$(aws ec2 \
      describe-security-groups --group-ids "$group" --query 'SecurityGroups[0].IpPermissions' --output json)" \
      >/dev/null 2>&1 || true
    aws ec2 authorize-security-group-ingress --group-id "$group" --protocol tcp --port 22 \
      --cidr "$me/32" >/dev/null
  fi
  # its host key under one name, whatever its address
  local ssh=(ssh -i "$key" -o HostKeyAlias=dirigent-test -o StrictHostKeyChecking=accept-new
    -o ServerAliveInterval=30 -o ConnectTimeout=5)
  timeout 300 bash -c "until $(printf '%q ' "${ssh[@]}") -o BatchMode=yes root@$ip true 2>/dev/null; do sleep 3; done"
  rsync -az --delete -e "${ssh[*]}" --exclude=/target/ --exclude=/.direnv/ --exclude=/.dnvr/ \
    --exclude=/.envrc --exclude=/result --exclude=/.env --exclude=/web/node_modules/ \
    --exclude=/web/dist/ --exclude=/web/chlive-client/ "$root/" "root@$ip:/root/dirigent/"
  local status=0
  "${ssh[@]}" "root@$ip" "cd /root/dirigent && NIX_CONFIG='warn-dirty = false' nix develop -c kube-test $(printf '%q ' "$@")" ||
    status=$?
  mkdir -p "$dir/runs"
  rsync -az -e "${ssh[*]}" "root@$ip:/root/dirigent/.dnvr/k3s/runs/" "$dir/runs/" 2>/dev/null || true
  return "$status"
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
  install) shift; install "$@" ;;
  load) load ;;
  e2e) shift; e2e "$@" ;;
  gc) gc ;;
  remote) shift; remote "$@" ;;
  down) down ;;
  kubectl) shift; exec kubectl "$@" ;;
  helm) shift; exec helm "$@" ;;
  *) sed -n '1,/^$/p' "$0" >&2; exit 2 ;;
esac
