-- openmig:generated

-- Create table "dirigent"."insert_kinds"
CREATE TABLE "dirigent"."insert_kinds" ("subject" text NOT NULL, "kind" text NOT NULL, CONSTRAINT "insert_kinds_pkey" PRIMARY KEY ("subject", "kind"));
-- Add column "role" to table "dirigent"."session_access"
ALTER TABLE "dirigent"."session_access" ADD COLUMN "role" text;
-- Each session's player keeps its lease; its least other subject becomes its owner, the rest its viewers (hand-written)
UPDATE "dirigent"."session_access" a SET "role" = CASE
  WHEN a."player" THEN 'player'
  WHEN a."subject" = (SELECT min(o."subject") FROM "dirigent"."session_access" o WHERE o."session_id" = a."session_id" AND NOT o."player") THEN 'owner'
  ELSE 'viewer' END;
ALTER TABLE "dirigent"."session_access" ALTER COLUMN "role" SET NOT NULL;
-- Create index "session_access_owner_idx" on "dirigent"."session_access"
CREATE UNIQUE INDEX "session_access_owner_idx" ON "dirigent"."session_access" USING btree ("session_id") WHERE (role = 'owner'::text);
-- Add check constraint "session_access_role_check" of "dirigent"."session_access"
ALTER TABLE "dirigent"."session_access" ADD CONSTRAINT "session_access_role_check" CHECK (role = ANY (ARRAY['owner'::text, 'viewer'::text, 'player'::text]));
-- Create function "dirigent"."grant_insert_kinds"()
CREATE FUNCTION "dirigent"."grant_insert_kinds"() RETURNS trigger LANGUAGE plpgsql VOLATILE CALLED ON NULL INPUT SECURITY DEFINER PARALLEL UNSAFE COST 100 SET search_path TO 'dirigent', 'pg_catalog' AS $$DECLARE
  who text;
  wanted text[];
  inputs text[] := ARRAY['user_message', 'user_interrupt'];
  every_kind text[] := inputs || ARRAY[
    'player_attached', 'player_detached', 'player_failed', 'workspace_ready',
    'workspace_restored', 'checkpoint', 'checkpoint_failed', 'turn_started', 'steered',
    'model_delta', 'assistant_message', 'model_cancelled', 'error', 'compacted',
    'compaction_failed', 'tool_result', 'turn_ended', 'session_update'];
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
-- Create trigger "session_access_insert_kinds" of "dirigent"."session_access"
CREATE TRIGGER "session_access_insert_kinds" AFTER INSERT OR UPDATE OR DELETE ON "dirigent"."session_access" FOR EACH ROW EXECUTE FUNCTION "dirigent"."grant_insert_kinds"();
-- Change the tables of publication "dirigent_chlive"
ALTER PUBLICATION "dirigent_chlive" ADD TABLE "dirigent"."insert_kinds";
-- Grant on table "dirigent"."insert_kinds" to "dirigent_chlive"
GRANT SELECT ON TABLE "dirigent"."insert_kinds" TO "dirigent_chlive";
-- Drop check constraint "session_access_lease_check" of "dirigent"."session_access"
ALTER TABLE "dirigent"."session_access" DROP CONSTRAINT "session_access_lease_check";
-- Add check constraint "session_access_lease_check" of "dirigent"."session_access"
ALTER TABLE "dirigent"."session_access" ADD CONSTRAINT "session_access_lease_check" CHECK (role = 'player'::text OR expires_at IS NULL AND spawned_by IS NULL);
-- Drop index "session_access_player_idx" of "dirigent"."session_access"
DROP INDEX "dirigent"."session_access_player_idx";
-- Create index "session_access_player_idx" on "dirigent"."session_access"
CREATE UNIQUE INDEX "session_access_player_idx" ON "dirigent"."session_access" USING btree ("session_id") WHERE (role = 'player'::text);
-- Drop column "player" of "dirigent"."session_access"
ALTER TABLE "dirigent"."session_access" DROP COLUMN "player";
-- The kinds the subjects there are may insert, by the trigger (hand-written)
UPDATE "dirigent"."session_access" SET "role" = "role";
