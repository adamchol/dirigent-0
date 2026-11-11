-- openmig:generated

-- Add column "archived" to table "dirigent"."sessions"
ALTER TABLE "dirigent"."sessions" ADD COLUMN "archived" timestamp with time zone;
