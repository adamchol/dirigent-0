-- openmig:generated

-- Create table "dirigent"."sessions"
CREATE TABLE "dirigent"."sessions" ("id" uuid NOT NULL, "epoch" bigint DEFAULT 0 NOT NULL, CONSTRAINT "sessions_pkey" PRIMARY KEY ("id"));
-- Create table "dirigent"."session_access"
CREATE TABLE "dirigent"."session_access" ("session_id" uuid NOT NULL, "subject" text NOT NULL, "player" boolean DEFAULT false NOT NULL, "expires_at" timestamp with time zone, CONSTRAINT "session_access_pkey" PRIMARY KEY ("session_id", "subject"), CONSTRAINT "session_access_lease_check" CHECK (player OR expires_at IS NULL), CONSTRAINT "session_access_session_id_fkey" FOREIGN KEY ("session_id") REFERENCES "dirigent"."sessions" ("id") ON DELETE CASCADE);
-- Create index "session_access_player_idx" on "dirigent"."session_access"
CREATE UNIQUE INDEX "session_access_player_idx" ON "dirigent"."session_access" USING btree ("session_id") WHERE (player);
-- Create publication "dirigent_chlive"
CREATE PUBLICATION "dirigent_chlive" FOR TABLE "dirigent"."session_access";
-- Grant on table "dirigent"."session_access" to "dirigent_chlive"
GRANT SELECT ON TABLE "dirigent"."session_access" TO "dirigent_chlive";
-- Grant on table "dirigent"."session_access" to "dirigent_control"
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE "dirigent"."session_access" TO "dirigent_control";
-- Grant on table "dirigent"."sessions" to "dirigent_control"
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE "dirigent"."sessions" TO "dirigent_control";
