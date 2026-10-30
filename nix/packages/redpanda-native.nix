# The `redpanda` dnvr process on Linux: Redpanda (Kafka API + HTTP proxy) run natively, its data
# in `dataDir` (relative to DNVR_ROOT); reset it together with ClickHouse (`reset-data`), since the
# sink's offsets in ClickHouse must match Kafka's. Picks its ports once, keeping them with the data,
# and publishes `bootstrap`, `proxyUrl` and `topics` once the topics exist.
{
  lib,
  writeShellApplication,
  coreutils,
  gnugrep,
  callPackage,
  dnvrState,
  name,
  topics,
  partitions,
  dataDir,
}:
let
  server = callPackage ./redpanda-server.nix { };
in
writeShellApplication {
  inherit name;
  runtimeInputs = [
    coreutils
    gnugrep
    dnvrState
    server
  ];
  text = ''
    dir="$DNVR_ROOT/${dataDir}"
    mkdir -p "$dir"
    # Picked once and kept with the data: clients resolve `bootstrap` only when they start, so
    # a restarted Redpanda must come back on the same ports.
    ports="$dir/ports"
    if [ ! -s "$ports" ]; then
      for _ in kafka proxy admin rpc registry; do dnvr-state pick-port; done | paste -sd' ' > "$ports"
    fi
    read -r kafka proxy admin rpc registry < "$ports"
    cfg="$dir/redpanda.yaml"
    rm -f "$cfg"
    rpk redpanda config bootstrap --self 127.0.0.1 --config "$cfg" >/dev/null
    rpk redpanda config set redpanda.data_directory "$dir/data" --config "$cfg"
    rpk redpanda config set redpanda.admin "[{address: 127.0.0.1, port: $admin}]" --config "$cfg"
    # The HTTP proxy batches produce requests for 100 ms by default; disable that so a
    # single write is acknowledged, and visible, immediately.
    rpk redpanda config set pandaproxy_client.produce_batch_delay_ms 0 --config "$cfg"
    rpk redpanda config set pandaproxy_client.brokers "[{address: 127.0.0.1, port: $kafka}]" --config "$cfg"

    rpk redpanda start --config "$cfg" --install-dir ${server} --mode dev-container \
      --smp 1 --memory 1G --check=false \
      --kafka-addr "127.0.0.1:$kafka" --advertise-kafka-addr "127.0.0.1:$kafka" \
      --pandaproxy-addr "127.0.0.1:$proxy" --advertise-pandaproxy-addr "127.0.0.1:$proxy" \
      --rpc-addr "127.0.0.1:$rpc" --advertise-rpc-addr "127.0.0.1:$rpc" \
      --schema-registry-addr "127.0.0.1:$registry" &
    pid=$!
    trap 'kill $pid 2>/dev/null || true; wait $pid 2>/dev/null || true' EXIT INT TERM

    brokers=(-X "brokers=127.0.0.1:$kafka" -X "admin.hosts=127.0.0.1:$admin")
    until rpk cluster health "''${brokers[@]}" 2>/dev/null | grep -qE 'Healthy:.+true'; do
      kill -0 $pid 2>/dev/null || { echo "[redpanda] exited before becoming ready" >&2; exit 1; }
      sleep 1
    done
    topics=(${lib.escapeShellArgs topics})
    for t in "''${topics[@]}"; do
      rpk topic create "''${brokers[@]}" "$t" -p ${toString partitions} -r 1 >/dev/null 2>&1 || true
    done
    dnvr-state set bootstrap "127.0.0.1:$kafka"
    dnvr-state set proxyUrl "http://127.0.0.1:$proxy"
    # readiness key: topics exist
    dnvr-state set topics ${lib.concatStringsSep "," topics}
    echo "[redpanda] ready: kafka 127.0.0.1:$kafka, http proxy 127.0.0.1:$proxy"
    wait $pid
  '';
}
