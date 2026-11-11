-- openmig:generated

-- Create table "dirigent"."web_sessions"
CREATE TABLE "dirigent"."web_sessions" ("id" uuid NOT NULL, "subject" text NOT NULL, "secret" bytea NOT NULL, "created" timestamp with time zone DEFAULT now() NOT NULL, "expires" timestamp with time zone NOT NULL, CONSTRAINT "web_sessions_pkey" PRIMARY KEY ("id"));
-- Grant on table "dirigent"."web_sessions" to "dirigent_control"
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE "dirigent"."web_sessions" TO "dirigent_control";
