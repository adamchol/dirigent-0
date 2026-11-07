-- openmig:generated

-- Create table "dirigent"."chatgpt_links"
CREATE TABLE "dirigent"."chatgpt_links" ("subject" text NOT NULL, "account" text NOT NULL, "email" text NOT NULL, "client_id" text NOT NULL, "id_token" text NOT NULL, "refresh_token" bytea NOT NULL, "broken" text, "updated" timestamp with time zone DEFAULT now() NOT NULL, CONSTRAINT "chatgpt_links_pkey" PRIMARY KEY ("subject"));
-- Grant on table "dirigent"."chatgpt_links" to "dirigent_control"
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE "dirigent"."chatgpt_links" TO "dirigent_control";
