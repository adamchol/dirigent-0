-- openmig:generated

-- Create role "dirigent_chlive_read"
CREATE ROLE IF NOT EXISTS `dirigent_chlive_read` IN `local_directory`;
-- Create role "dirigent_chlive_snapshot"
CREATE ROLE IF NOT EXISTS `dirigent_chlive_snapshot` IN `local_directory`;
-- Create database "chlive_snapshots"
CREATE DATABASE IF NOT EXISTS `chlive_snapshots` ENGINE = Atomic;
-- Create database "dirigent"
CREATE DATABASE IF NOT EXISTS `dirigent` ENGINE = Atomic;
-- Create table "dirigent"."dirigent_sink_session_events_state"
CREATE TABLE IF NOT EXISTS `dirigent`.`dirigent_sink_session_events_state`
(
    `key` String,
    `minOffset` Int64,
    `maxOffset` Int64,
    `state` String
)
ENGINE = KeeperMap('/dirigent-sink/session_events')
PRIMARY KEY key;
-- Create table "dirigent"."session_events"
CREATE TABLE IF NOT EXISTS `dirigent`.`session_events`
(
    `session_id` UUID,
    `offset` UInt64,
    `partition` UInt32,
    `ts` DateTime64(3),
    `id` UUID,
    `author` LowCardinality(String),
    `kind` LowCardinality(String),
    `data` String
)
ENGINE = MergeTree
ORDER BY (session_id, offset)
TTL toDateTime(ts) + toIntervalDay(1) WHERE kind = 'model_delta'
SETTINGS `index_granularity` = 8192;
-- Grant on "chlive_snapshots".* to "dirigent_chlive_read"
GRANT SELECT ON `chlive_snapshots`.* TO `dirigent_chlive_read`;
-- Grant on "dirigent".* to "dirigent_chlive_read"
GRANT SELECT ON `dirigent`.* TO `dirigent_chlive_read`;
-- Grant on "chlive_snapshots".* to "dirigent_chlive_snapshot"
GRANT ALTER, CREATE TABLE, DROP TABLE, INSERT, SELECT ON `chlive_snapshots`.* TO `dirigent_chlive_snapshot`;
-- Grant on "dirigent".* to "dirigent_chlive_snapshot"
GRANT ALTER, CREATE TABLE, DROP TABLE, INSERT, SELECT, SYSTEM SYNC REPLICA ON `dirigent`.* TO `dirigent_chlive_snapshot`;
