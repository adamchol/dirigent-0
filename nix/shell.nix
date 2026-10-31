# The dnvr dev shell: the Rust toolchain, the local event system (nix/stack.nix: Redpanda,
# ClickHouse, Kafka Connect with the ClickHouse sink, the pause server, chlive), a player on the
# mock provider, and scripts.
{
  pkgs,
  presets,
  dnvrState,
  continuous-backup,
  clickhouse,
  chlive,
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
      ;
    topics = [ "session_events" ];
    schemaFile = "scripts/schema.sql";
    serverConfig = "nix/chlive.toml";
    cacheDir = ".dnvr/chlive-cache";
    # each row also stores its message's partition, offset and timestamp (scripts/schema.sql),
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
    export DIRIGENT_KAFKA="''${DIRIGENT_KAFKA:-$(dnvr-state get redpanda.bootstrap)}"
  '';
in
{ ... }:
{
  description = "dirigent: a player on the mock provider over chlive (Redpanda + ClickHouse + Kafka Connect + pause server + chlive)";

  defaultRunner = "rex";

  packages = with pkgs; [
    cargo
    rustc
    clippy
    rustfmt
    rust-analyzer
    # the rdkafka crate links librdkafka dynamically
    pkg-config
    rdkafka
    zlib
    jq
    curl
    nixfmt
    dnvrState
  ];

  env = {
    RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
    # the session the dev player runs and `chat` opens
    DIRIGENT_SESSION = "d1a1e000-0000-4000-8000-000000000001";
  };

  inherit (stack) prerun;

  processes = stack.processes // {
    # A player for $DIRIGENT_SESSION, working in .dnvr/workdir. On the mock provider unless
    # DIRIGENT_PLAYER_ARGS says otherwise, e.g. `DIRIGENT_PLAYER_ARGS= dnvr up` for the real one.
    player = {
      env = {
        CHLIVE_URL = "dnvr://chlive/url";
        DIRIGENT_KAFKA = "dnvr://redpanda/bootstrap";
      };
      command = ''
        cd "$DNVR_ROOT"
        mkdir -p .dnvr/workdir
        # shellcheck disable=SC2086
        exec cargo run -q -p dirigent-player -- --workdir .dnvr/workdir ''${DIRIGENT_PLAYER_ARGS---mock}
      '';
    };
  };

  scripts = stack.scripts // {
    chat = {
      description = "The terminal client on $DIRIGENT_SESSION (or --session <uuid>)";
      runtimeInputs = [ dnvrState ];
      text = ''
        ${stackEnv}
        cd "$DNVR_ROOT"
        exec cargo run -q -p dirigent-chat -- "$@"
      '';
    };
    e2e = {
      description = "Run the player's tests against the running stack (mock provider)";
      runtimeInputs = [ dnvrState ];
      text = ''
        ${stackEnv}
        cd "$DNVR_ROOT"
        DIRIGENT_E2E=1 exec cargo test -p dirigent-player e2e "$@"
      '';
    };
  };
}
