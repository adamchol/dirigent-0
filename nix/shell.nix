# The dnvr dev shell: the Rust toolchain, the local event system (nix/stack.nix: Redpanda,
# ClickHouse, Kafka Connect with the ClickHouse sink, the pause server, chlive, Postgres), the
# control plane running the started sessions' players (mock provider), with a dev session, and
# scripts.
{
  pkgs,
  presets,
  dnvrState,
  continuous-backup,
  clickhouse,
  chlive,
  openmig,
  postgresql,
}:
let
  stack = import ./stack.nix {
    inherit
      pkgs
      presets
      dnvrState
      continuous-backup
      clickhouse
      chlive
      openmig
      postgresql
      ;
    topics = [ "session_events" ];
    database = "dirigent";
    accessFile = "scripts/clickhouse-dev-access.sql";
    serverConfig = "nix/chlive.toml";
    cacheDir = ".dnvr/chlive-cache";
    # the checkpoints' object store and kopia repository, and control's players' sessions
    # (workspaces and players' state)
    extraData = [
      ".dnvr/rustfs"
      ".dnvr/kopia"
      ".dnvr/sessions"
    ];
    # each row also stores its message's partition, offset and timestamp (db/clickhouse),
    # which chlive reads from the messages for live rows (nix/chlive.toml)
    connectorSettings.session_events = {
      transforms = "kafka";
      "transforms.kafka.type" = "org.apache.kafka.connect.transforms.InsertField$Value";
      "transforms.kafka.partition.field" = "partition";
      "transforms.kafka.offset.field" = "offset";
      "transforms.kafka.timestamp.field" = "ts";
    };
  };

  # For scripts run in the shell: the running stack's addresses; a value already set wins.
  stackEnv = ''
    export CHLIVE_URL="''${CHLIVE_URL:-$(dnvr-state get chlive.url)}"
    export DIRIGENT_CLICKHOUSE_URL="''${DIRIGENT_CLICKHOUSE_URL:-$(dnvr-state get clickhouse.httpUrl | sed 's|://|://default:@|')}"
    export DIRIGENT_POSTGRES_URL="''${DIRIGENT_POSTGRES_URL:-$(dnvr-state get postgres.url)}"
    export DIRIGENT_CONTROL_POSTGRES="''${DIRIGENT_CONTROL_POSTGRES:-$(dnvr-state get postgres.url | sed 's|://[^@]*@|://dirigent_control@|')}"
    export DIRIGENT_CONTROL_HTTP="''${DIRIGENT_CONTROL_HTTP:-$(dnvr-state get control.url)}"
    export DIRIGENT_CONTROL_URL="''${DIRIGENT_CONTROL_URL:-$(dnvr-state get control.grpcUrl)}"
    export DIRIGENT_KOPIA_TOKEN="''${DIRIGENT_KOPIA_TOKEN:-$(dnvr-state get kopia-repo.token)}"
  '';
in
{ ... }:
{
  description = "dirigent: the control plane running players (mock provider) over chlive (Redpanda + ClickHouse + Kafka Connect + pause server + chlive + Postgres)";

  defaultRunner = "rex";

  packages = with pkgs; [
    cargo
    rustc
    clippy
    rustfmt
    rust-analyzer
    pkg-config
    jq
    curl
    # the player clones sessions' sources with git, and checkpoints workspaces with kopia (to
    # RustFS, S3)
    git
    kopia
    rustfs
    nixfmt
    dnvrState
    # migrations (openmig.toml); its scratch servers run these, the same majors as the stack's
    openmig
    postgresql
    clickhouse
  ];

  env = {
    RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
    # the session the dev player runs and `chat` opens, as `user:dev` (chat's default)
    DIRIGENT_SESSION = "d1a1e000-0000-4000-8000-000000000001";
  };

  inherit (stack) prerun;

  processes = stack.processes // {
    # The control plane, as Postgres' `dirigent_control` role, running the started sessions'
    # players as local processes, each in .dnvr/sessions/<id> (its output in player.log). On the
    # mock provider unless DIRIGENT_PLAYER_ARGS says otherwise, e.g. `DIRIGENT_PLAYER_ARGS= dnvr
    # up` for the real one. Publishes `url` (its HTTP API) and `grpcUrl` (where players hold their
    # sessions). Its ports are fixed: players reconnect to the same address when it restarts.
    control = {
      env = {
        PG_URL = "dnvr://postgres/url";
        MIGRATED = "dnvr://migrate-postgres/ready";
        # for the players
        CHLIVE_URL = "dnvr://chlive/url";
        DIRIGENT_KOPIA_TOKEN = "dnvr://kopia-repo/token";
      };
      command = ''
        cd "$DNVR_ROOT"
        port=54330
        dnvr-state set url "http://127.0.0.1:$port"
        grpc=54331
        dnvr-state set grpcUrl "http://127.0.0.1:$grpc"
        cargo build -q -p dirigent-control -p dirigent-player
        # exec'd, not `cargo run`, which wouldn't pass the stop signal on
        # shellcheck disable=SC2086
        exec target/debug/control --listen "127.0.0.1:$port" --grpc-listen "127.0.0.1:$grpc" \
          --postgres "''${PG_URL/:\/\/*@/://dirigent_control@}" \
          --players .dnvr/sessions --player target/debug/player \
          -- ''${DIRIGENT_PLAYER_ARGS---mock}
      '';
    };

    # RustFS: the S3 store workspaces are checkpointed to (through kopia-repo). Publishes `url`.
    rustfs = {
      command = ''
        cd "$DNVR_ROOT"
        mkdir -p .dnvr/rustfs
        port=$(dnvr-state pick-port)
        dnvr-state set url "http://127.0.0.1:$port"
        exec rustfs server --address "127.0.0.1:$port" \
          --access-key dirigent --secret-key dirigent-dev-secret .dnvr/rustfs
      '';
    };

    # The kopia repository on RustFS, made once, with the checkpoints' policy: `.gitignore`d files
    # and files over 50 MB are left out, and no snapshot ever expires (the log refers to them).
    # Publishes `token`, which players connect with; then idles.
    kopia-repo = {
      env.S3 = "dnvr://rustfs/url";
      command = ''
        cd "$DNVR_ROOT"
        mkdir -p .dnvr/kopia
        export KOPIA_CONFIG_PATH=.dnvr/kopia/admin.config KOPIA_LOG_DIR=.dnvr/kopia/logs \
          KOPIA_CHECK_FOR_UPDATES=false
        # the bucket; RustFS answers 503 for a moment after it's up
        timeout 60 sh -c "until curl -fsS --aws-sigv4 aws:amz:us-east-1:s3 \
          --user dirigent:dirigent-dev-secret -X PUT '$S3/dirigent-workspaces' -o /dev/null \
          || curl -fsS --aws-sigv4 aws:amz:us-east-1:s3 --user dirigent:dirigent-dev-secret \
          -I '$S3/dirigent-workspaces' -o /dev/null; do sleep 1; done"
        s3=(--bucket dirigent-workspaces --endpoint "''${S3#http://}" --disable-tls
          --access-key dirigent --secret-access-key dirigent-dev-secret --password dirigent-dev)
        if ! kopia repository connect s3 "''${s3[@]}" >/dev/null 2>&1; then
          kopia repository create s3 "''${s3[@]}"
        fi
        kopia policy set --global --add-dot-ignore .gitignore --max-file-size 50000000 \
          --keep-latest 1000000000 --keep-hourly 0 --keep-daily 0 --keep-weekly 0 \
          --keep-monthly 0 --keep-annual 0 >/dev/null
        token=$(kopia repository status -t -s | grep -o -- '--token [^ ]*' | cut -d' ' -f2)
        dnvr-state set token "$token"
        dnvr-state set ready 1
        echo "[kopia-repo] ready"
        exec tail -f /dev/null
      '';
    };

    # $DIRIGENT_SESSION, made and started through control with `user:dev` its user; then idles.
    dev-session = {
      env.CONTROL = "dnvr://control/url";
      command = ''
        curl -fsS --retry 60 --retry-connrefused --retry-delay 1 \
          -X PUT "$CONTROL/sessions/$DIRIGENT_SESSION" \
          -H 'content-type: application/json' -d '{"users": ["user:dev"], "start": true}'
        dnvr-state set ready 1
        echo "[dev-session] $DIRIGENT_SESSION"
        exec tail -f /dev/null
      '';
    };
  };

  scripts = stack.scripts // {
    chat = {
      description = "The terminal client on $DIRIGENT_SESSION (or --session <uuid>), as user:dev (or --user <subject>)";
      runtimeInputs = [ dnvrState ];
      text = ''
        ${stackEnv}
        cd "$DNVR_ROOT"
        exec cargo run -q -p dirigent-chat -- "$@"
      '';
    };
    session = {
      description = "Make a session through control: session [--start] [--git <url> [--ref <ref>]] [--model <m>] [--effort <e>] [<uuid> [<user>]] (a new id, user:dev); prints its id. Started, control starts a player for it";
      runtimeInputs = [
        dnvrState
        pkgs.curl
        pkgs.jq
        pkgs.util-linux
      ];
      text = ''
        ${stackEnv}
        spec='{}'
        while [ $# -gt 0 ]; do
          case "$1" in
            --start) spec=$(jq -c '.start = true' <<<"$spec"); shift ;;
            --git) spec=$(jq -c --arg u "$2" '.source.git.url = $u' <<<"$spec"); shift 2 ;;
            --ref) spec=$(jq -c --arg r "$2" '.source.git.ref = $r' <<<"$spec"); shift 2 ;;
            --model) spec=$(jq -c --arg m "$2" '.model = $m' <<<"$spec"); shift 2 ;;
            --effort) spec=$(jq -c --arg e "$2" '.effort = $e' <<<"$spec"); shift 2 ;;
            *) break ;;
          esac
        done
        id="''${1:-$(uuidgen)}"
        body=$(jq -c --arg u "''${2:-user:dev}" '.users = [$u]' <<<"$spec")
        curl -fsS -X PUT "$DIRIGENT_CONTROL_HTTP/sessions/$id" \
          -H 'content-type: application/json' -d "$body"
        echo "$id"
      '';
    };
    sessions = {
      description = "The sessions control has: id, epoch and the player holding it (- when idle)";
      runtimeInputs = [
        dnvrState
        pkgs.curl
        pkgs.jq
      ];
      text = ''
        ${stackEnv}
        curl -fsS "$DIRIGENT_CONTROL_HTTP/sessions" | jq -r \
          '.[] | [.id, .epoch, (.player | if . then .subject + (if .attached then "" else " (away)" end) else "-" end)] | @tsv'
      '';
    };
    start = {
      description = "Start (or resume) a session: start <uuid>; control starts a player for it, unless one holds it";
      runtimeInputs = [
        dnvrState
        pkgs.curl
      ];
      text = ''
        ${stackEnv}
        curl -fsS -X POST "$DIRIGENT_CONTROL_HTTP/sessions/''${1:?a session id}/start"
      '';
    };
    stop = {
      description = "Stop a session: stop [--now] <uuid>; its player finishes the turn (--now: cancels it) and detaches";
      runtimeInputs = [
        dnvrState
        pkgs.curl
      ];
      text = ''
        ${stackEnv}
        now=false
        if [ "''${1:-}" = --now ]; then
          now=true
          shift
        fi
        curl -fsS -X POST "$DIRIGENT_CONTROL_HTTP/sessions/''${1:?a session id}/stop" \
          -H 'content-type: application/json' -d "{\"now\": $now}"
      '';
    };
    e2e = {
      description = "Run the control plane's and the player's tests against the running stack (mock provider)";
      runtimeInputs = [ dnvrState ];
      text = ''
        ${stackEnv}
        cd "$DNVR_ROOT"
        # control's tests run it
        cargo build -q -p dirigent-player
        DIRIGENT_E2E=1 cargo test -p dirigent-control e2e "$@"
        DIRIGENT_E2E=1 exec cargo test -p dirigent-player e2e "$@"
      '';
    };
  };
}
