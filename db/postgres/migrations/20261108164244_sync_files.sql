-- openmig:generated

-- Replace function "dirigent"."grant_insert_kinds"()
CREATE OR REPLACE FUNCTION "dirigent"."grant_insert_kinds"() RETURNS trigger LANGUAGE plpgsql VOLATILE CALLED ON NULL INPUT SECURITY DEFINER PARALLEL UNSAFE COST 100 SET search_path TO 'dirigent', 'pg_catalog' AS $$DECLARE
  who text;
  wanted text[];
  inputs text[] := ARRAY['user_message', 'user_interrupt', 'titled'];
  every_kind text[] := inputs || ARRAY[
    'player_attached', 'player_detached', 'player_failed', 'workspace_ready',
    'workspace_restored', 'checkpoint', 'checkpoint_failed', 'turn_started', 'steered',
    'model_delta', 'assistant_message', 'model_cancelled', 'error', 'compacted',
    'compaction_failed', 'tool_result', 'turn_ended', 'session_update', 'files_changed'];
BEGIN
  FOREACH who IN ARRAY ARRAY[OLD.subject, NEW.subject] LOOP
    CONTINUE WHEN who IS NULL;
    wanted := CASE
      WHEN EXISTS (SELECT FROM session_access WHERE subject = who AND role = 'player')
        THEN every_kind
      WHEN EXISTS (SELECT FROM session_access WHERE subject = who AND role = 'owner')
        THEN inputs
      ELSE ARRAY[]::text[]
    END;
    -- as it should be, whatever another transaction did meanwhile
    INSERT INTO insert_kinds (subject, kind) SELECT who, unnest(wanted)
      ON CONFLICT DO NOTHING;
    DELETE FROM insert_kinds WHERE subject = who AND kind <> ALL (wanted);
  END LOOP;
  RETURN NULL;
END
$$;
