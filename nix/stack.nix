# The local event system, as dnvr processes, after chlive's own stack (chlive's nix/stack.nix):
# Redpanda (native on Linux, Docker elsewhere), ClickHouse with an embedded Keeper, Kafka
# Connect with one ClickHouse sink connector per topic (exactly once), the pause server, the
# chlive server, and Postgres; both databases get their migrations (openmig.toml) on start.
#
# Every process picks its own ports (`dnvr-state pick-port`) and publishes its address; the
# others get it through `dnvr://<process>/<key>` env refs, which also order the start-up.
{
  pkgs,
  presets,
  dnvrState,
  continuous-backup,
  clickhouse,
  chlive,
  openmig,
  postgresql,
  topics,
  # per topic; a session's events are keyed by its id, so they stay in one partition
  partitions ? 3,
  # the ClickHouse database the topics' tables are in (made by the migrations)
  database,
  # after the ClickHouse migrations, what a deployer would do: chlive's users, with passwords and
  # settings, given the migrations' roles; relative to DNVR_ROOT
  accessFile,
  # Postgres' port, fixed (the preset doesn't pick one)
  postgresPort ? 54320,
  # the chlive server's config file, relative to DNVR_ROOT (addresses come as flags)
  serverConfig,
  # the chlive server's result store (`cache.dir` in its config), for `reset-data`
  cacheDir,
  # per topic, more settings for its sink connector (e.g. transforms)
  connectorSettings ? { },
  # more data directories for `reset-data` to delete, relative to DNVR_ROOT
  extraData ? [ ],
  # more env for the chlive server, e.g. refs to what its config needs first
  chliveEnv ? { },
}:
let
  dir = n: ".dnvr/${n}";

  native = pkgs.stdenv.hostPlatform.isLinux;
  redpanda =
    if native then
      pkgs.callPackage ./packages/redpanda-native.nix {
        inherit dnvrState topics partitions;
        name = "dirigent-redpanda";
        dataDir = dir "redpanda";
      }
    else
      pkgs.callPackage ./packages/redpanda.nix {
        inherit dnvrState topics partitions;
        name = "dirigent-redpanda";
      };
  resetRedpanda =
    if native then
      ''rm -rf "''${DNVR_ROOT:?}/${dir "redpanda"}"''
    else
      "docker rm -f dirigent-redpanda >/dev/null 2>&1 || true; docker volume rm dirigent-redpanda >/dev/null 2>&1 || true";

  cb = continuous-backup;
  # continuous-backup's binary; `pause-server <config>` is the pause server
  backup = (pkgs.callPackage "${cb}/nix/backup.nix" { }).overrideAttrs (_: {
    doCheck = false;
  });
  sinkPlugin = pkgs.callPackage "${cb}/nix/clickhouse-kafka-connect.nix" { };
  supervise = import "${cb}/nix/supervise.nix" { inherit (pkgs) lib; };

  # The connector keeps its offsets in a KeeperMap table, which the pause server reads.
  sinkConnector = topic: {
    name = "dirigent-sink-${topic}";
    stateTable = "dirigent_sink_${topic}_state";
    keeperPath = "/dirigent-sink/${topic}";
  };
  # ClickHouse's `hostname` and `port` are added at runtime, once it has published them.
  connectorConfig =
    topic:
    let
      c = sinkConnector topic;
    in
    pkgs.writeText "${c.name}.json" (
      builtins.toJSON (
        {
          "connector.class" = "com.clickhouse.kafka.connect.ClickHouseSinkConnector";
          "tasks.max" = "1";
          topics = topic;
          topic2TableMap = "${topic}=${topic}";
          ssl = "false";
          inherit database;
          username = "default";
          password = "";
          exactlyOnce = "true";
          "consumer.override.isolation.level" = "read_committed";
          "errors.tolerance" = "all";
          "errors.deadletterqueue.topic.name" = "dirigent-deadletterqueue";
          "errors.deadletterqueue.topic.replication.factor" = "1";
          "errors.deadletterqueue.context.headers.enable" = "true";
          bufferCount = "0";
          zkPath = c.keeperPath;
          zkDatabase = c.stateTable;
          "value.converter" = "org.apache.kafka.connect.json.JsonConverter";
          "value.converter.schemas.enable" = "false";
          clickhouseSettings = "async_insert=0,insert_deduplicate=1";
        }
        // (connectorSettings.${topic} or { })
      )
    );
  # `bootstrap.servers` and `listeners` are appended at runtime.
  connectProperties = pkgs.writeText "dirigent-connect.properties" ''
    group.id=dirigent-connect
    config.storage.topic=dirigent-connect-configs
    offset.storage.topic=dirigent-connect-offsets
    status.storage.topic=dirigent-connect-status
    config.storage.replication.factor=1
    offset.storage.replication.factor=1
    status.storage.replication.factor=1
    key.converter=org.apache.kafka.connect.storage.StringConverter
    value.converter=org.apache.kafka.connect.json.JsonConverter
    value.converter.schemas.enable=false
    plugin.path=${sinkPlugin}/plugins
    connector.client.config.override.policy=All
    offset.flush.interval.ms=1000
  '';
  # `listen`, `connectUrl`, `clickhouseUrl` and `keeperAddress` are added at runtime.
  pauseServerConfig = pkgs.writeText "dirigent-pause-server.json" (
    builtins.toJSON {
      keeperPath = "/dirigent-pause-server";
      pauseTimeoutSeconds = 30;
      pipelines = map (
        topic:
        let
          c = sinkConnector topic;
        in
        {
          connector = c.name;
          inherit database;
          state_table = c.stateTable;
          keeper_path = c.keeperPath;
          table = topic;
          inherit topic partitions;
        }
      ) topics;
      timeouts = {
        clickhouseConnectSeconds = 10;
        connectConnectSeconds = 10;
        connectRequestSeconds = 30;
        connectPollSeconds = 1;
        kafkaMetadataSeconds = 10;
        kafkaCatalogReadSeconds = 15;
        kafkaTransactionSeconds = 30;
        kafkaMaxPollSeconds = 86400;
        kafkaReplayPollSeconds = 15;
        recoveryCatchupSeconds = 3600;
        controllerRetrySeconds = 15;
      };
    }
  );

  # `clickhouse client` on the local ClickHouse's native port, as the default user, with its own
  # config file so ~/.clickhouse-client/config.xml never applies.
  chClient =
    let
      config = pkgs.writeText "dirigent-clickhouse-client.xml" ''
        <config>
          <host>127.0.0.1</host>
          <user>default</user>
          <password></password>
          <secure>false</secure>
        </config>
      '';
    in
    port: "${clickhouse}/bin/clickhouse client --config-file ${config} --port ${port}";
in
{
  inherit chClient;

  # The ports of ClickHouse's embedded Keeper: ClickHouse's config reads them with `from_env`,
  # and the pause server connects to it.
  prerun = ''
    DIRIGENT_KEEPER_PORT=$(dnvr-state pick-port)
    DIRIGENT_KEEPER_RAFT_PORT=$(dnvr-state pick-port)
    export DIRIGENT_KEEPER_PORT DIRIGENT_KEEPER_RAFT_PORT
  '';

  processes = {
    # Publishes `bootstrap`, `proxyUrl` and `topics` once the topics exist.
    redpanda.command = redpanda;

    # Publishes `host`, `httpPort`, `tcpPort` and `httpUrl`, then `database` once it answers.
    clickhouse = {
      imports = [ presets.clickhouse ];
      package = clickhouse;
      database = "default";
      httpPort = null;
      tcpPort = null;
      dataDir = dir "clickhouse";
      timezone = "UTC";
      extraConfigXml = ''
        <access_control_path>${dir "clickhouse"}/access/</access_control_path>
        <keeper_server>
          <tcp_port from_env="DIRIGENT_KEEPER_PORT"/>
          <server_id>1</server_id>
          <log_storage_path>${dir "clickhouse"}/coordination/log</log_storage_path>
          <snapshot_storage_path>${dir "clickhouse"}/coordination/snapshots</snapshot_storage_path>
          <raft_configuration>
            <server>
              <id>1</id>
              <hostname>127.0.0.1</hostname>
              <port from_env="DIRIGENT_KEEPER_RAFT_PORT"/>
            </server>
          </raft_configuration>
        </keeper_server>
        <zookeeper>
          <node><host>127.0.0.1</host><port from_env="DIRIGENT_KEEPER_PORT"/></node>
        </zookeeper>
        <keeper_map_path_prefix>/clickhouse/keeper_map</keeper_map_path_prefix>
      '';
    };

    # The pending migrations (openmig.toml), as ClickHouse's admin, then chlive's users
    # (accessFile); both are no-ops when done. Then idles so its `ready` key stays live.
    migrate-clickhouse = {
      env = {
        CH_TCP_PORT = "dnvr://clickhouse/tcpPort";
        CH_HTTP_URL = "dnvr://clickhouse/httpUrl";
        CH_DATABASE = "dnvr://clickhouse/database";
      };
      command = ''
        cd "$DNVR_ROOT"
        DIRIGENT_CLICKHOUSE_URL="''${CH_HTTP_URL/:\/\//://default:@}" \
          ${openmig}/bin/openmig -d clickhouse deploy --wait-for-db 1m
        ${chClient ''"$CH_TCP_PORT"''} --multiquery < ${accessFile}
        dnvr-state set ready 1
        echo "[migrate-clickhouse] migrated, and applied ${accessFile}"
        exec tail -f /dev/null
      '';
    };

    # The control plane's state (db/postgres); logical WAL for chlive's grants (CDC).
    # Publishes `url` and `database` once the database exists.
    postgres = {
      imports = [ presets.postgres ];
      package = postgresql;
      database = "dirigent";
      port = postgresPort;
      dataDir = dir "postgres";
      # initdb refuses root: started as root (e.g. a dev VM), it runs as this user
      runAsUser = "postgres";
      settings = {
        wal_level = "logical";
        max_replication_slots = 10;
        max_wal_senders = 10;
      };
    };

    # The pending migrations (openmig.toml), as Postgres' superuser; then idles.
    migrate-postgres = {
      env.DIRIGENT_POSTGRES_URL = "dnvr://postgres/url";
      command = ''
        cd "$DNVR_ROOT"
        ${openmig}/bin/openmig -d postgres deploy --wait-for-db 1m
        dnvr-state set ready 1
        echo "[migrate-postgres] migrated"
        exec tail -f /dev/null
      '';
    };

    connect = {
      env.KAFKA = "dnvr://redpanda/bootstrap";
      command = ''
        cd "$DNVR_ROOT"
        mkdir -p ${dir "connect"}
        port=$(dnvr-state pick-port)
        props=${dir "connect"}/connect.properties
        {
          cat ${connectProperties}
          echo "bootstrap.servers=$KAFKA"
          echo "listeners=http://127.0.0.1:$port"
        } > "$props"
        ${supervise {
          name = "Kafka Connect";
          # one small sink connector: 512 MiB is plenty (its default is a 2 GiB heap)
          start = ''${pkgs.coreutils}/bin/env KAFKA_HEAP_OPTS="-Xms128M -Xmx512M" ${pkgs.apacheKafka}/bin/connect-distributed.sh "$props"'';
          readyWhen = ''${pkgs.curl}/bin/curl -fs "http://127.0.0.1:$port/connector-plugins" >/dev/null'';
          onReady = ''
            dnvr-state set url "http://127.0.0.1:$port"
            echo "[connect] ready"
          '';
        }}
      '';
    };

    # One sink connector per topic (created or updated), once the tables exist.
    connectors = {
      env = {
        CONNECT_URL = "dnvr://connect/url";
        MIGRATED = "dnvr://migrate-clickhouse/ready";
        TOPICS = "dnvr://redpanda/topics";
        CH_HOST = "dnvr://clickhouse/host";
        CH_HTTP_PORT = "dnvr://clickhouse/httpPort";
      };
      command = ''
        ${pkgs.lib.concatMapStrings (
          topic:
          let
            c = sinkConnector topic;
          in
          ''
            ${pkgs.jq}/bin/jq --arg host "$CH_HOST" --arg port "$CH_HTTP_PORT" \
              '. + {hostname: $host, port: $port}' ${connectorConfig topic} \
              | ${pkgs.curl}/bin/curl -sS --fail-with-body -X PUT \
                -H 'Content-Type: application/json' --data-binary @- \
                "$CONNECT_URL/connectors/${c.name}/config" >/dev/null
            # a stop during a snapshot leaves the sink paused, and a task that failed on old
            # settings stays failed: start clean
            ${pkgs.curl}/bin/curl -sS --fail-with-body -X PUT \
              "$CONNECT_URL/connectors/${c.name}/resume" >/dev/null
            ${pkgs.curl}/bin/curl -sS --fail-with-body -X POST \
              "$CONNECT_URL/connectors/${c.name}/restart?includeTasks=true&onlyFailed=true" >/dev/null
            until ${pkgs.curl}/bin/curl -fs "$CONNECT_URL/connectors/${c.name}/status" \
              | ${pkgs.jq}/bin/jq -e '.connector.state == "RUNNING" and (.tasks | length > 0) and all(.tasks[]; .state == "RUNNING")' >/dev/null; do
              sleep 0.5
            done
            echo "[connectors] ${c.name} running"
          ''
        ) topics}
        dnvr-state set ready 1
        exec tail -f /dev/null
      '';
    };

    # Pauses a table's ingestion while chlive snapshots it.
    pause-server = {
      env = {
        CONNECTORS_READY = "dnvr://connectors/ready";
        CONNECT_URL = "dnvr://connect/url";
        CLICKHOUSE_URL = "dnvr://clickhouse/httpUrl";
      };
      command = ''
        cd "$DNVR_ROOT"
        mkdir -p ${dir "pause-server"}
        port=$(dnvr-state pick-port)
        conf=${dir "pause-server"}/config.json
        ${pkgs.jq}/bin/jq --arg listen "127.0.0.1:$port" \
          --arg connect "$CONNECT_URL" --arg clickhouse "$CLICKHOUSE_URL" \
          --arg keeper "127.0.0.1:''${DIRIGENT_KEEPER_PORT:?}" \
          '. + {listen: $listen, connectUrl: $connect, clickhouseUrl: $clickhouse, keeperAddress: $keeper}' \
          ${pauseServerConfig} > "$conf"
        export CLICKHOUSE_USERNAME=default CLICKHOUSE_PASSWORD=
        ${supervise {
          name = "Pause server";
          start = ''${backup}/bin/durable-clickhouse-backup pause-server "$conf"'';
          # leading, not just up: a standby answers /health too, but refuses the rest with 503,
          # where the leader gives an unknown token 404
          readyWhen = ''[ "$(${pkgs.curl}/bin/curl -s -o /dev/null -w '%{http_code}' -H 'Content-Type: application/json' -d '{"token":"00000000-0000-0000-0000-000000000000"}' "http://127.0.0.1:$port/renew")" = 404 ]'';
          onReady = ''
            dnvr-state set url "http://127.0.0.1:$port"
            echo "[pause-server] ready"
          '';
        }}
      '';
    };

    # Publishes `url` (the WebSocket) and `httpUrl` (/debug, /healthz, …).
    # Its grants come from Postgres (the config's `pg` provider), as the `dirigent_chlive` role.
    chlive = {
      env = {
        PAUSE_SERVER_URL = "dnvr://pause-server/url";
        CH_HTTP_URL = "dnvr://clickhouse/httpUrl";
        KAFKA_BROKERS = "dnvr://redpanda/bootstrap";
        PG_URL = "dnvr://postgres/url";
        MIGRATED = "dnvr://migrate-postgres/ready";
      }
      // chliveEnv;
      command = ''
        cd "$DNVR_ROOT"
        port=$(dnvr-state pick-port)
        dnvr-state set url "ws://127.0.0.1:$port/live"
        dnvr-state set httpUrl "http://127.0.0.1:$port"
        exec ${chlive}/bin/chlive --config ${serverConfig} \
          --listen "127.0.0.1:$port" --clickhouse-url "$CH_HTTP_URL" \
          --kafka "$KAFKA_BROKERS" --pause-server "$PAUSE_SERVER_URL" \
          --set "security.providers.pg.url=''${PG_URL/:\/\/*@/://dirigent_chlive@}"
      '';
    };
  };

  scripts = {
    ch = {
      description = "clickhouse client connected to the local ClickHouse";
      runtimeInputs = [ dnvrState ];
      text = ''exec ${chClient ''"$(dnvr-state get clickhouse.tcpPort)"''} "$@"'';
    };
    psql = {
      description = "psql connected to the local Postgres (the dirigent database)";
      runtimeInputs = [ dnvrState ];
      text = ''exec ${postgresql}/bin/psql "$(dnvr-state get postgres.url)" "$@"'';
    };
    reset-data = {
      description = "Delete the local Redpanda, ClickHouse, Postgres and chlive cache data, and the stack's own (run while `dnvr up` is stopped)";
      text = ''
        # Kafka (with Connect's own topics) and ClickHouse (with the sink's KeeperMap offsets)
        # go together; chlive's stored results hold cursors into both.
        ${resetRedpanda}
        rm -rf "''${DNVR_ROOT:?}/${dir "clickhouse"}" "''${DNVR_ROOT:?}/${cacheDir}" \
          "''${DNVR_ROOT:?}/${dir "postgres"}" ${
            pkgs.lib.concatMapStringsSep " " (d: "\"\${DNVR_ROOT:?}/${d}\"") extraData
          }
        echo "removed Redpanda, ClickHouse, Postgres and chlive cache data; the next \`dnvr up\` starts empty"
      '';
    };
  };
}
