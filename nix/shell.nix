# The dnvr dev shell: the Rust toolchain, the local event system (nix/stack.nix: Redpanda,
# ClickHouse, Kafka Connect with the ClickHouse sink, the pause server, chlive, Postgres), the
# control plane with a dev session, a player for it on the mock provider, and scripts.
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
    # the checkpoints' object store and kopia repository, and the dev player's kopia state
    extraData = [
      ".dnvr/rustfs"
      ".dnvr/kopia"
      ".dnvr/player-state"
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
    export DIRIGENT_CONTROL_URL="''${DIRIGENT_CONTROL_URL:-$(dnvr-state get control.wsUrl)}"
    export DIRIGENT_KOPIA_TOKEN="''${DIRIGENT_KOPIA_TOKEN:-$(dnvr-state get kopia-repo.token)}"
  '';
in
{ ... }:
{
  description = "dirigent: the control plane and a player on the mock provider over chlive (Redpanda + ClickHouse + Kafka Connect + pause server + chlive + Postgres)";

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
    # The control plane, as Postgres' `dirigent_control` role. Publishes `url` (its HTTP API)
    # and `wsUrl` (players' WebSocket). Its port is fixed: players reconnect to the same address
    # when it restarts.
    control = {
      env = {
        PG_URL = "dnvr://postgres/url";
        MIGRATED = "dnvr://migrate-postgres/ready";
      };
      command = ''
        cd "$DNVR_ROOT"
        port=54330
        dnvr-state set url "http://127.0.0.1:$port"
        dnvr-state set wsUrl "ws://127.0.0.1:$port/player"
        cargo build -q -p dirigent-control
        # exec'd, not `cargo run`, which wouldn't pass the stop signal on
        exec target/debug/control --listen "127.0.0.1:$port" \
          --postgres "''${PG_URL/:\/\/*@/://dirigent_control@}"
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

    # $DIRIGENT_SESSION, made through control with `user:dev` its user; then idles.
    dev-session = {
      env.CONTROL = "dnvr://control/url";
      command = ''
        curl -fsS --retry 60 --retry-connrefused --retry-delay 1 \
          -X PUT "$CONTROL/sessions/$DIRIGENT_SESSION" \
          -H 'content-type: application/json' -d '{"users": ["user:dev"]}'
        dnvr-state set ready 1
        echo "[dev-session] $DIRIGENT_SESSION"
        exec tail -f /dev/null
      '';
    };

    # A player for $DIRIGENT_SESSION, working in .dnvr/workdir. On the mock provider unless
    # DIRIGENT_PLAYER_ARGS says otherwise, e.g. `DIRIGENT_PLAYER_ARGS= dnvr up` for the real one.
    player = {
      env = {
        CHLIVE_URL = "dnvr://chlive/url";
        DIRIGENT_CONTROL_URL = "dnvr://control/wsUrl";
        SESSION_READY = "dnvr://dev-session/ready";
        DIRIGENT_KOPIA_TOKEN = "dnvr://kopia-repo/token";
      };
      command = ''
        cd "$DNVR_ROOT"
        mkdir -p .dnvr/workdir
        cargo build -q -p dirigent-player
        # exec'd, not `cargo run`, which wouldn't pass SIGTERM on: the lease would drop instead of
        # being given back
        # shellcheck disable=SC2086
        exec target/debug/player --workdir .dnvr/workdir --state-dir .dnvr/player-state ''${DIRIGENT_PLAYER_ARGS---mock}
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
      description = "Make a session through control: session [--git <url> [--ref <ref>]] [--model <m>] [--effort <e>] [<uuid> [<user>]] (a new id, user:dev); prints its id. Run a player on it with `cargo run -p dirigent-player -- --mock --session <id> --workdir <dir>`";
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
    e2e = {
      description = "Run the control plane's and the player's tests against the running stack (mock provider)";
      runtimeInputs = [ dnvrState ];
      text = ''
        ${stackEnv}
        cd "$DNVR_ROOT"
        DIRIGENT_E2E=1 cargo test -p dirigent-control e2e "$@"
        DIRIGENT_E2E=1 exec cargo test -p dirigent-player e2e "$@"
      '';
    };
  };
}
