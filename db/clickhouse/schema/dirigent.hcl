# dirigent's ClickHouse: the sessions' events, ingested from Kafka by Kafka Connect's ClickHouse
# sink (exactly once), each message one row of the table named like its topic; and the roles
# chlive reads them with. The users with these roles are the deployer's.

database "dirigent" {}

# Where chlive makes its snapshots (clones of the tables): `clickhouse.snapshot_database`.
database "chlive_snapshots" {}

# A session: everything its users and its player write, in the order Kafka took it (`offset`).
# `offset`, `partition` and `ts` are the message's, added by the sink's InsertField transform
# (nix/shell.nix); `id` is set by the writer, `kind` is the event's type and `data` its fields.
# Streaming deltas are for live display only, so they expire after a day.
table "dirigent.session_events" {
  engine = MergeTree
  column "session_id" { type = UUID }
  column "offset" { type = UInt64 }
  column "partition" { type = UInt32 }
  column "ts" { type = DateTime64(3) }
  column "id" { type = UUID }
  column "author" { type = LowCardinality(String) }
  column "kind" { type = LowCardinality(String) }
  column "data" { type = String }
  order_by = [column.session_id, column.offset]
  ttl      = "toDateTime(ts) + toIntervalDay(1) WHERE kind = 'model_delta'"
}

# The sink's exactly-once state (the connector's `zkDatabase` / `zkPath`), as continuous-backup
# creates it: per partition (`<topic>-<p>`), the offsets of the last insert and its state.
table "dirigent.dirigent_sink_session_events_state" {
  engine = KeeperMap("/dirigent-sink/session_events")
  column "key" { type = String }
  column "minOffset" { type = Int64 }
  column "maxOffset" { type = Int64 }
  column "state" { type = String }
  primary_key = [column.key]
}

# chlive's reads (`clickhouse.user`): history, from the tables and their snapshots. It also needs
# SELECT on system.tables, which the deployer grants (openmig doesn't plan grants on `system`).
role "dirigent_chlive_read" {}

grant {
  privileges = [SELECT]
  on         = [database.dirigent, database.chlive_snapshots]
  to         = [role.dirigent_chlive_read]
}

# chlive's snapshots (`clickhouse.snapshot_user`): clones made with CREATE TABLE … AS and
# ATTACH PARTITION ALL FROM, and dropped when they're replaced.
role "dirigent_chlive_snapshot" {}

grant {
  privileges = [SELECT, INSERT, "CREATE TABLE", "DROP TABLE", ALTER]
  on         = [database.dirigent, database.chlive_snapshots]
  to         = [role.dirigent_chlive_snapshot]
}

grant {
  privileges = ["SYSTEM SYNC REPLICA"]
  on         = [database.dirigent]
  to         = [role.dirigent_chlive_snapshot]
}
