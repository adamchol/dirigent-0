-- ClickHouse schema, applied by the dnvr `schema` process on every start (idempotent).
-- Ingestion is Kafka Connect's ClickHouse sink, exactly once: each message is one row of the
-- table of the same name. The topic is keyed by session_id, so a session stays in one partition.

-- A session: everything its users and its player write, in the order Kafka took it (`offset`).
-- `offset`, `partition` and `ts` are the message's, added by the sink's InsertField transform
-- (nix/shell.nix); `id` is set by the writer, `kind` is the event's type and `data` its fields.
-- Streaming deltas are for live display only, so they expire after a day.
CREATE TABLE IF NOT EXISTS default.session_events (
  session_id UUID,
  offset UInt64,
  partition UInt32,
  ts DateTime64(3),
  id UUID,
  author LowCardinality(String),
  kind LowCardinality(String),
  data String
) ENGINE = MergeTree ORDER BY (session_id, offset)
TTL toDateTime(ts) + INTERVAL 1 DAY DELETE WHERE kind = 'model_delta';

-- The sink's exactly-once state (the connector's `zkDatabase` / `zkPath`), as continuous-backup
-- creates it: per partition (`<topic>-<p>`), the offsets of the last insert and its state.
CREATE TABLE IF NOT EXISTS default.dirigent_sink_session_events_state
  (key String, minOffset Int64, maxOffset Int64, state String)
  ENGINE = KeeperMap('/dirigent-sink/session_events') PRIMARY KEY key;

-- The chlive server's users: chlive_ro reads (history, from snapshots), chlive_snap makes the
-- snapshots (clones, with CREATE TABLE … AS + ATTACH PARTITION ALL FROM).
-- readonly=2: read-only, but per-query settings are allowed (the engine sends output formats).
CREATE USER IF NOT EXISTS chlive_ro IDENTIFIED WITH plaintext_password BY 'chlive_ro'
  SETTINGS readonly = 2, max_execution_time = 60, max_memory_usage = 4000000000;
GRANT SELECT ON default.* TO chlive_ro;
GRANT SELECT ON system.tables TO chlive_ro;

CREATE USER IF NOT EXISTS chlive_snap IDENTIFIED WITH plaintext_password BY 'chlive_snap';
GRANT SELECT, INSERT, CREATE TABLE, DROP TABLE, ALTER ON default.* TO chlive_snap;
GRANT SYSTEM SYNC REPLICA ON default.* TO chlive_snap;
GRANT SELECT ON system.tables TO chlive_snap;

CREATE DATABASE IF NOT EXISTS chlive_snapshots;
GRANT SELECT, INSERT, CREATE TABLE, DROP TABLE, ALTER ON chlive_snapshots.* TO chlive_snap;
GRANT SELECT ON chlive_snapshots.* TO chlive_ro;
