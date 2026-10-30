-- ClickHouse schema, applied by the dnvr `schema` process on every start (idempotent).
-- Ingestion is Kafka Connect's ClickHouse sink, one connector per topic, exactly once: each
-- message is one row of the table of the same name. Every topic is keyed by session_id.

-- A session's log, written only by its player: every event it handled, in order (`seq`).
-- `kind` is the event's type and `data` its fields, as JSON.
CREATE TABLE IF NOT EXISTS default.session_events (
  session_id UUID, seq UInt64, ts DateTime64(3), kind LowCardinality(String), data String
) ENGINE = MergeTree ORDER BY (session_id, seq);

-- What users send to a session (`kind`: message or interrupt). The player logs each one it
-- takes, with its input_id, so on resume it knows which are left.
CREATE TABLE IF NOT EXISTS default.session_inputs (
  session_id UUID, input_id UUID, ts DateTime64(3), kind LowCardinality(String), text String
) ENGINE = MergeTree ORDER BY (session_id, ts);

-- Streaming deltas of model replies, for live display only. `after_seq` is the seq of the last
-- logged event when the delta was sent, which ties it to the model call it belongs to.
CREATE TABLE IF NOT EXISTS default.session_deltas (
  session_id UUID, after_seq UInt64, ts DateTime64(3), data String
) ENGINE = MergeTree ORDER BY (session_id, ts) TTL toDateTime(ts) + INTERVAL 1 DAY;

-- The sink's exactly-once state (the connectors' `zkDatabase` / `zkPath`), as continuous-backup
-- creates it: per partition (`<topic>-<p>`), the offsets of the last insert and its state.
CREATE TABLE IF NOT EXISTS default.dirigent_sink_session_events_state
  (key String, minOffset Int64, maxOffset Int64, state String)
  ENGINE = KeeperMap('/dirigent-sink/session_events') PRIMARY KEY key;
CREATE TABLE IF NOT EXISTS default.dirigent_sink_session_inputs_state
  (key String, minOffset Int64, maxOffset Int64, state String)
  ENGINE = KeeperMap('/dirigent-sink/session_inputs') PRIMARY KEY key;
CREATE TABLE IF NOT EXISTS default.dirigent_sink_session_deltas_state
  (key String, minOffset Int64, maxOffset Int64, state String)
  ENGINE = KeeperMap('/dirigent-sink/session_deltas') PRIMARY KEY key;

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
