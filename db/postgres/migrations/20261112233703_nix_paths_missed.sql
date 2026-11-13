-- openmig:generated

-- Add column "missed" to table "dirigent"."nix_paths"
ALTER TABLE "dirigent"."nix_paths" ADD COLUMN "missed" integer DEFAULT 0 NOT NULL;
