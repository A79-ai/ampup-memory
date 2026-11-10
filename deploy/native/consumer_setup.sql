-- One-time consumer setup for the Memory app. Run in a Snowsight worksheet as ACCOUNTADMIN (a
-- session that is itself PAT-authenticated can't mint another role's PAT); replace MEMORY_APP with
-- the installed application's name. Creates the database the memory lives in, a service user whose
-- role can create schemas in it (and owns only those), and that user's PAT stored straight into a
-- secret: never displayed, and Snowflake redacts SECRET_STRING in query history.

CREATE DATABASE IF NOT EXISTS MEMORY_DATA CATALOG = 'SNOWFLAKE' EXTERNAL_VOLUME = 'SNOWFLAKE_MANAGED';
CREATE ROLE IF NOT EXISTS MEMORY_ENGINE;
GRANT USAGE, CREATE SCHEMA ON DATABASE MEMORY_DATA TO ROLE MEMORY_ENGINE;
CREATE USER IF NOT EXISTS MEMORY_ENGINE TYPE = SERVICE DEFAULT_ROLE = MEMORY_ENGINE
  COMMENT = 'Memory app engine: reads/writes MEMORY_DATA through the Iceberg REST catalog';
GRANT ROLE MEMORY_ENGINE TO USER MEMORY_ENGINE;

-- Service-user PATs need a network policy; the engine runs inside Snowflake (SPCS) with no fixed
-- egress IP, so this user's PAT skips network-policy evaluation. Scope: this one user.
CREATE AUTHENTICATION POLICY IF NOT EXISTS MEMORY_DATA.PUBLIC.MEMORY_ENGINE_AUTH
  PAT_POLICY = (NETWORK_POLICY_EVALUATION = NOT_ENFORCED);
ALTER USER MEMORY_ENGINE SET AUTHENTICATION POLICY MEMORY_DATA.PUBLIC.MEMORY_ENGINE_AUTH;

EXECUTE IMMEDIATE $$
DECLARE
  pat STRING;
BEGIN
  ALTER USER MEMORY_ENGINE REMOVE PROGRAMMATIC ACCESS TOKEN IF EXISTS MEMORY_APP_PAT;
  ALTER USER MEMORY_ENGINE ADD PROGRAMMATIC ACCESS TOKEN MEMORY_APP_PAT
    ROLE_RESTRICTION = 'MEMORY_ENGINE' DAYS_TO_EXPIRY = 365 COMMENT = 'Memory app engine';
  SELECT "token_secret" INTO :pat FROM TABLE(RESULT_SCAN(LAST_QUERY_ID()));
  EXECUTE IMMEDIATE 'CREATE OR REPLACE SECRET MEMORY_DATA.PUBLIC.MEMORY_ENGINE_PAT TYPE = GENERIC_STRING SECRET_STRING = ''' || pat || '''';
  RETURN 'PAT stored in MEMORY_DATA.PUBLIC.MEMORY_ENGINE_PAT (expires in 365 days)';
END
$$;

-- Read access for analysts (optional; the engine role owns the schemas it creates):
--   GRANT USAGE ON DATABASE MEMORY_DATA TO ROLE <analyst role>;
--   GRANT USAGE ON FUTURE SCHEMAS IN DATABASE MEMORY_DATA TO ROLE <analyst role>;
--   GRANT SELECT ON FUTURE ICEBERG TABLES IN DATABASE MEMORY_DATA TO ROLE <analyst role>;
-- Cortex for extraction and answers (only the Cortex role, not the whole SNOWFLAKE database):
GRANT DATABASE ROLE SNOWFLAKE.CORTEX_USER TO APPLICATION MEMORY_APP;

-- Hand the credential to the app, provision it, then approve its "storage_access" specification
-- (Snowsight: Data Products > Apps > MEMORY_APP > Security, or SHOW SPECIFICATIONS / APPROVE).
CALL MEMORY_APP.core.register_reference('ENGINE_CREDENTIAL', 'ADD',
  SYSTEM$REFERENCE('SECRET', 'MEMORY_DATA.PUBLIC.MEMORY_ENGINE_PAT', 'PERSISTENT', 'READ'));
CALL MEMORY_APP.core.provision('MEMORY_DATA', 'MEMORY_ENGINE');

-- After approving STORAGE_ACCESS: provision once more (restarts the engine with egress allowed),
-- then wait for CALL MEMORY_APP.core.engine_status() to show READY.
CALL MEMORY_APP.core.provision('MEMORY_DATA', 'MEMORY_ENGINE');
CALL MEMORY_APP.core.schedule_ingest('qwen3-32b', 15);   -- ingest whatever is remembered, every 15 min
CALL MEMORY_APP.core.mcp_url();                          -- the MCP endpoint for CoCo / Claude / Cursor
