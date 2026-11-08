-- openmig:generated

-- Create table "dirigent"."api_keys"
CREATE TABLE "dirigent"."api_keys" ("subject" text NOT NULL, "provider" text NOT NULL, "key" bytea NOT NULL, "hint" text NOT NULL, "updated" timestamp with time zone DEFAULT now() NOT NULL, CONSTRAINT "api_keys_pkey" PRIMARY KEY ("subject", "provider"));
-- Create table "dirigent"."session_secrets"
CREATE TABLE "dirigent"."session_secrets" ("session_id" uuid NOT NULL, "name" text NOT NULL, "value" bytea NOT NULL, "updated" timestamp with time zone DEFAULT now() NOT NULL, CONSTRAINT "session_secrets_pkey" PRIMARY KEY ("session_id", "name"), CONSTRAINT "session_secrets_session_id_fkey" FOREIGN KEY ("session_id") REFERENCES "dirigent"."sessions" ("id") ON DELETE CASCADE);
-- Create table "dirigent"."user_secrets"
CREATE TABLE "dirigent"."user_secrets" ("subject" text NOT NULL, "name" text NOT NULL, "value" bytea NOT NULL, "updated" timestamp with time zone DEFAULT now() NOT NULL, CONSTRAINT "user_secrets_pkey" PRIMARY KEY ("subject", "name"));
-- Replace function "dirigent"."grant_insert_kinds"()
CREATE OR REPLACE FUNCTION "dirigent"."grant_insert_kinds"() RETURNS trigger LANGUAGE plpgsql VOLATILE CALLED ON NULL INPUT SECURITY DEFINER PARALLEL UNSAFE COST 100 SET search_path TO 'dirigent', 'pg_catalog' AS $$DECLARE
  who text;
  wanted text[];
  inputs text[] := ARRAY['user_message', 'user_interrupt', 'titled', 'secret_answered'];
  every_kind text[] := inputs || ARRAY[
    'player_attached', 'player_detached', 'player_failed', 'workspace_ready',
    'workspace_restored', 'checkpoint', 'checkpoint_failed', 'turn_started', 'steered',
    'model_delta', 'assistant_message', 'model_cancelled', 'error', 'compacted',
    'compaction_failed', 'tool_result', 'turn_ended', 'session_update', 'files_changed',
    'secrets_changed'];
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
-- Grant on table "dirigent"."api_keys" to "dirigent_control"
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE "dirigent"."api_keys" TO "dirigent_control";
-- Grant on table "dirigent"."session_secrets" to "dirigent_control"
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE "dirigent"."session_secrets" TO "dirigent_control";
-- Grant on table "dirigent"."user_secrets" to "dirigent_control"
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE "dirigent"."user_secrets" TO "dirigent_control";
