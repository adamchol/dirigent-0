-- openmig:generated

-- Add column "spec" to table "dirigent"."sessions"
ALTER TABLE "dirigent"."sessions" ADD COLUMN "spec" jsonb DEFAULT '{}'::jsonb NOT NULL;
