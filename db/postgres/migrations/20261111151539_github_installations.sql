-- openmig:generated

-- Create table "dirigent"."github_installations"
CREATE TABLE "dirigent"."github_installations" ("subject" text NOT NULL, "installation_id" bigint NOT NULL, "account" text NOT NULL, "linked" timestamp with time zone DEFAULT now() NOT NULL, CONSTRAINT "github_installations_pkey" PRIMARY KEY ("subject", "installation_id"));
-- Grant on table "dirigent"."github_installations" to "dirigent_control"
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE "dirigent"."github_installations" TO "dirigent_control";
