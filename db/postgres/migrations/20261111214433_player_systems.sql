-- openmig:generated

-- Create table "dirigent"."player_systems"
CREATE TABLE "dirigent"."player_systems" ("hash" text NOT NULL, "state" text NOT NULL, "build_id" uuid NOT NULL, "token" text NOT NULL, "store_path" text, "error" text, "started" timestamp with time zone DEFAULT now() NOT NULL, "updated" timestamp with time zone DEFAULT now() NOT NULL, CONSTRAINT "player_systems_pkey" PRIMARY KEY ("hash"), CONSTRAINT "player_systems_state" CHECK (state = ANY (ARRAY['building'::text, 'ready'::text, 'failed'::text, 'broken'::text])));
-- Grant on table "dirigent"."player_systems" to "dirigent_control"
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE "dirigent"."player_systems" TO "dirigent_control";
