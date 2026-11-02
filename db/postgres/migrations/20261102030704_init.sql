-- openmig:generated

-- Create role "dirigent_chlive"
DO $$BEGIN IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname = 'dirigent_chlive') THEN CREATE ROLE "dirigent_chlive" WITH LOGIN REPLICATION; ELSE ALTER ROLE "dirigent_chlive" WITH LOGIN REPLICATION; END IF; END$$;
-- Create role "dirigent_control"
DO $$BEGIN IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname = 'dirigent_control') THEN CREATE ROLE "dirigent_control" WITH LOGIN; ELSE ALTER ROLE "dirigent_control" WITH LOGIN; END IF; END$$;
-- Create schema "dirigent"
CREATE SCHEMA "dirigent";
-- Grant on schema "dirigent" to "dirigent_chlive"
GRANT USAGE ON SCHEMA "dirigent" TO "dirigent_chlive";
-- Grant on schema "dirigent" to "dirigent_control"
GRANT USAGE ON SCHEMA "dirigent" TO "dirigent_control";
