-- Add column "created" to table "dirigent"."sessions". The sessions made before it get the Unix
-- epoch, so clients list them by their activity, not as just made.
ALTER TABLE "dirigent"."sessions" ADD COLUMN "created" timestamp with time zone DEFAULT 'epoch' NOT NULL;
ALTER TABLE "dirigent"."sessions" ALTER COLUMN "created" SET DEFAULT now();
