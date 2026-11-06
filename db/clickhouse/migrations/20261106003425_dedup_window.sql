-- openmig:generated

-- Modify settings of "dirigent"."session_events"
ALTER TABLE `dirigent`.`session_events` MODIFY SETTING `non_replicated_deduplication_window` = 1000;
