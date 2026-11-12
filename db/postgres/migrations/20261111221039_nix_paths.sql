-- openmig:generated

-- Create table "dirigent"."nix_paths"
CREATE TABLE "dirigent"."nix_paths" ("path" text NOT NULL, "signed" boolean NOT NULL, "uses" integer DEFAULT 1 NOT NULL, "first_seen" timestamp with time zone DEFAULT now() NOT NULL, "promoted_in" bigint, CONSTRAINT "nix_paths_pkey" PRIMARY KEY ("path"));
-- Grant on table "dirigent"."nix_paths" to "dirigent_control"
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE "dirigent"."nix_paths" TO "dirigent_control";
