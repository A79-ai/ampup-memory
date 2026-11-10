-- Connect AI agents (Claude, Claude Cowork, Cortex Code, Cursor) to the Memory app through a Snowflake-managed MCP
-- server. Run as ACCOUNTADMIN in a Snowsight worksheet after the app is installed and provisioned.
-- Replace MEMORY_APP with the app's name and COMPUTE_WH with the warehouse the tools should run on.

-- 1. A role for people (and their agents) who may read the memory.
CREATE ROLE IF NOT EXISTS MEMORY_USER;
GRANT APPLICATION ROLE MEMORY_APP.APP_USER TO ROLE MEMORY_USER;
GRANT USAGE ON WAREHOUSE COMPUTE_WH TO ROLE MEMORY_USER;
GRANT USAGE ON DATABASE MEMORY_DATA TO ROLE MEMORY_USER;
GRANT USAGE ON SCHEMA MEMORY_DATA.PUBLIC TO ROLE MEMORY_USER;
-- GRANT ROLE MEMORY_USER TO USER <user>;  ALTER USER <user> SET DEFAULT_ROLE = MEMORY_USER DEFAULT_WAREHOUSE = COMPUTE_WH;

-- 2. The MCP server: three tools over the app's functions.
CREATE OR REPLACE MCP SERVER MEMORY_DATA.PUBLIC.MEMORY_MCP FROM SPECIFICATION $$
tools:
  - name: "search_memory"
    identifier: "MEMORY_APP.API.SEARCH"
    type: "GENERIC"
    description: "Full-text search over the team memory's facts (decisions, issues, owners, how things work). Returns the best matches as JSON with entity, predicate, value, quote, valid_from and the ids of the source episodes each fact cites."
    config:
      type: "function"
      warehouse: "COMPUTE_WH"
      query_timeout: 60
      input_schema:
        type: "object"
        properties:
          q: {type: "string", description: "Words to search for"}
          k: {type: "integer", description: "How many facts (at most 50)"}
  - name: "get_episode"
    identifier: "MEMORY_APP.API.EPISODE"
    type: "GENERIC"
    description: "The source text of an episode (a Slack thread, PR, call or doc) that a fact cites, at most max_chars characters, centred on `around` (pass the fact's quote). Null if missing or not visible."
    config:
      type: "function"
      warehouse: "COMPUTE_WH"
      query_timeout: 60
      input_schema:
        type: "object"
        properties:
          episode_id: {type: "string", description: "An id from a fact's source_episodes"}
          around: {type: "string", description: "Text to centre the excerpt on, usually the fact's quote"}
          max_chars: {type: "integer", description: "Excerpt size, e.g. 4000"}
  - name: "ask_memory"
    identifier: "MEMORY_APP.CORE.ASK"
    type: "GENERIC"
    description: "Answer a question from the team memory with Cortex, using the best facts and excerpts of the episodes they cite; cites episodes."
    config:
      type: "procedure"
      warehouse: "COMPUTE_WH"
      query_timeout: 120
      input_schema:
        type: "object"
        properties:
          q: {type: "string", description: "The question"}
          model: {type: "string", description: "Cortex model, e.g. claude-sonnet-4-5"}
$$;
GRANT USAGE ON MCP SERVER MEMORY_DATA.PUBLIC.MEMORY_MCP TO ROLE MEMORY_USER;

-- Endpoint:
--   https://<org>-<account>.snowflakecomputing.com/api/v2/databases/MEMORY_DATA/schemas/PUBLIC/mcp-servers/MEMORY_MCP
-- (use hyphens, not underscores, in the account part of the hostname)

-- 3. Claude and Claude Cowork (custom connector, OAuth with PKCE). Then in Claude: Settings > Connectors >
--    Add custom connector, paste the endpoint and the client id/secret from SYSTEM$SHOW_OAUTH_CLIENT_SECRETS.
CREATE SECURITY INTEGRATION IF NOT EXISTS MEMORY_CLAUDE
  TYPE = OAUTH OAUTH_CLIENT = CUSTOM OAUTH_CLIENT_TYPE = 'PUBLIC' ENABLED = TRUE
  OAUTH_REDIRECT_URI = 'https://claude.ai/api/mcp/auth_callback'
  OAUTH_USE_SECONDARY_ROLES = NONE
  ALLOWED_ROLES_LIST = ('MEMORY_USER');
SELECT SYSTEM$SHOW_OAUTH_CLIENT_SECRETS('MEMORY_CLAUDE');

-- 4. Cortex Code (CoCo): a personal access token restricted to MEMORY_USER, kept in an environment variable:
--    cortex mcp add memory <endpoint> --type http -H "Authorization: Bearer ${SNOWFLAKE_PAT}"
