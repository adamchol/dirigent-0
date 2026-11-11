-- openmig:generated

-- Create table "dirigent"."user_settings"
CREATE TABLE "dirigent"."user_settings" ("subject" text NOT NULL, "settings" jsonb NOT NULL, "updated" timestamp with time zone DEFAULT now() NOT NULL, CONSTRAINT "user_settings_pkey" PRIMARY KEY ("subject"));
-- Grant on table "dirigent"."user_settings" to "dirigent_control"
GRANT SELECT, INSERT, UPDATE, DELETE ON TABLE "dirigent"."user_settings" TO "dirigent_control";
