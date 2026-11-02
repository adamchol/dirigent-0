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
      };
      command = ''
        cd "$DNVR_ROOT"
        mkdir -p .dnvr/workdir
        cargo build -q -p dirigent-player
        # exec'd, not `cargo run`, which wouldn't pass SIGTERM on: the lease would drop instead of
        # being given back
        # shellcheck disable=SC2086
        exec target/debug/player --workdir .dnvr/workdir ''${DIRIGENT_PLAYER_ARGS---mock}
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
      description = "Make a session through control: session [<uuid> [<user>]] (a new id, user:dev); prints its id";
      runtimeInputs = [
        dnvrState
        pkgs.curl
        pkgs.util-linux
      ];
      text = ''
        ${stackEnv}
        id="''${1:-$(uuidgen)}"
        curl -fsS -X PUT "$DIRIGENT_CONTROL_HTTP/sessions/$id" \
          -H 'content-type: application/json' -d "{\"users\": [\"''${2:-user:dev}\"]}"
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
