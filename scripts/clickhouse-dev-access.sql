-- The dev stack's ClickHouse users, applied by the dnvr `migrate-clickhouse` process after the
-- migrations (db/clickhouse), on every start (idempotent). This is the deployer's part: users with
-- their passwords and settings, given the roles the migrations made.

-- chlive's reads (`clickhouse.user` in nix/chlive.toml).
-- readonly=2: read-only, but per-query settings are allowed (the engine sends output formats).
CREATE USER OR REPLACE chlive_ro IDENTIFIED WITH plaintext_password BY 'chlive_ro'
  SETTINGS readonly = 2, max_execution_time = 60, max_memory_usage = 4000000000;
GRANT dirigent_chlive_read TO chlive_ro;
GRANT SELECT ON system.tables TO chlive_ro;

-- chlive's snapshots (`clickhouse.snapshot_user`).
CREATE USER OR REPLACE chlive_snap IDENTIFIED WITH plaintext_password BY 'chlive_snap';
GRANT dirigent_chlive_snapshot TO chlive_snap;
GRANT SELECT ON system.tables TO chlive_snap;
