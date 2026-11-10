# Memory

A governed team memory that lives entirely in your Snowflake account. Give it text (Slack threads,
pull requests, call transcripts, docs); Cortex extracts durable facts (decisions, issues, owners,
how things work, how to turn them on) into Apache Iceberg tables in a database **you** own; a search
engine in Snowpark Container Services serves them; Cortex answers questions from them, citing the
facts. Facts are time-versioned and cite the text they came from.

## Where it runs

Unlike a hosted memory service, everything runs natively in your account:
your Iceberg tables, your Cortex, your compute, your access controls.

## Install and set up (once, as ACCOUNTADMIN, in a Snowsight worksheet)

1. Create the memory's database and the engine's service user (the PAT is minted straight into a
   secret and never displayed). Replace `<app>` with this application's name:

```sql
CREATE DATABASE IF NOT EXISTS MEMORY_DATA CATALOG = 'SNOWFLAKE' EXTERNAL_VOLUME = 'SNOWFLAKE_MANAGED';
CREATE ROLE IF NOT EXISTS MEMORY_ENGINE;
GRANT USAGE, CREATE SCHEMA ON DATABASE MEMORY_DATA TO ROLE MEMORY_ENGINE;
CREATE USER IF NOT EXISTS MEMORY_ENGINE TYPE = SERVICE DEFAULT_ROLE = MEMORY_ENGINE;
GRANT ROLE MEMORY_ENGINE TO USER MEMORY_ENGINE;
CREATE AUTHENTICATION POLICY IF NOT EXISTS MEMORY_DATA.PUBLIC.MEMORY_ENGINE_AUTH
  PAT_POLICY = (NETWORK_POLICY_EVALUATION = NOT_ENFORCED);
ALTER USER MEMORY_ENGINE SET AUTHENTICATION POLICY MEMORY_DATA.PUBLIC.MEMORY_ENGINE_AUTH;
EXECUTE IMMEDIATE $$
DECLARE pat STRING;
BEGIN
  ALTER USER MEMORY_ENGINE ADD PROGRAMMATIC ACCESS TOKEN MEMORY_APP_PAT
    ROLE_RESTRICTION = 'MEMORY_ENGINE' DAYS_TO_EXPIRY = 365;
  SELECT "token_secret" INTO :pat FROM TABLE(RESULT_SCAN(LAST_QUERY_ID()));
  EXECUTE IMMEDIATE 'CREATE OR REPLACE SECRET MEMORY_DATA.PUBLIC.MEMORY_ENGINE_PAT TYPE = GENERIC_STRING SECRET_STRING = ''' || pat || '''';
  RETURN 'PAT stored';
END
$$;
```

2. Grant Cortex, bind the credential and provision:

```sql
GRANT DATABASE ROLE SNOWFLAKE.CORTEX_USER TO APPLICATION <app>;
CALL <app>.core.register_reference('ENGINE_CREDENTIAL', 'ADD',
  SYSTEM$REFERENCE('SECRET', 'MEMORY_DATA.PUBLIC.MEMORY_ENGINE_PAT', 'PERSISTENT', 'READ'));
CALL <app>.core.provision('MEMORY_DATA', 'MEMORY_ENGINE');
```

3. Approve the app's **STORAGE_ACCESS** specification (Snowsight: the app's Security tab), run
   `CALL <app>.core.provision('MEMORY_DATA', 'MEMORY_ENGINE');` once more (it restarts the engine with
   egress allowed), then wait for `CALL <app>.core.engine_status();` to show `READY`.

4. Grant `APPLICATION ROLE <app>.APP_USER` to roles that may read the memory and `APP_ADMIN` to roles
   that run ingestion and operations. Each reader sees only the memories whose access list names them
   (see **Who sees what**).

## Use

```sql
CALL <app>.core.remember('slack:#eng', 'We decided to ship the export button behind a flag ...', CURRENT_TIMESTAMP());
CALL <app>.core.ingest('qwen3-32b');                     -- extract with Cortex and commit (any Cortex model)
CALL <app>.core.ask('What did we decide about export?', 'qwen3-32b');        -- facts + source excerpts
CALL <app>.core.ask('What did we decide about export?', 'qwen3-32b', FALSE); -- facts only (cheaper)
SELECT <app>.api.search('export button', 10);            -- the raw facts, each with its quote
SELECT <app>.api.episode('<episode id>', NULL, 4000);    -- a cited episode's text
CALL <app>.core.stop();   CALL <app>.core.resume();      -- pause / resume the engine's compute
CALL <app>.core.schedule_ingest('qwen3-32b', 15);        -- ingest automatically (serverless task)
CALL <app>.core.unschedule_ingest();
CALL <app>.core.backfill_quotes();                       -- quotes for facts ingested before v0.5 patch 4
CALL <app>.core.backfill_quotes(TRUE);                   -- recompute every backfilled quote
```

## Who sees what

Every memory carries an access list. `core.remember(source, body, occurred_at, acl)` takes an array of
entries: `'role:<ROLE>'` (anyone holding that Snowflake role), `'user:<USER>'` (one Snowflake user),
`'group:<audience>'` or `'*'` (everyone). Unquoted names are stored upper-case, matching Snowflake; a
quoted, mixed-case name must be given exactly. Without an `acl`, the memory goes to the app's default
audience.

The app checks the caller itself; it holds no shared identity:

- **SQL** (`api.search`, `api.episode`, `core.ask`) sees `CURRENT_USER()` and the roles active in the
  session, so `USE ROLE` and `USE SECONDARY ROLES` change what a worksheet can read.
- **MCP** (the managed server and the app's own endpoint) runs as the Snowflake-verified caller: their
  user, their default role and every role granted to them. A user's first call after about two minutes
  idle takes 2 to 7 seconds longer while Snowflake confirms who they are.

Answers from `core.ask` use only the facts and sources the caller may see.

## Use from AI agents (Claude, Claude Cowork, Cortex Code, Cursor)

Agents reach the memory through a Snowflake-managed MCP server in your account, signed in as your Snowflake
users and limited to what their role may see. As ACCOUNTADMIN (replace `<app>` and `<warehouse>`):

```sql
CREATE ROLE IF NOT EXISTS MEMORY_USER;
GRANT APPLICATION ROLE <app>.APP_USER TO ROLE MEMORY_USER;
GRANT USAGE ON WAREHOUSE <warehouse> TO ROLE MEMORY_USER;
GRANT USAGE ON DATABASE MEMORY_DATA TO ROLE MEMORY_USER;
GRANT USAGE ON SCHEMA MEMORY_DATA.PUBLIC TO ROLE MEMORY_USER;

CREATE OR REPLACE MCP SERVER MEMORY_DATA.PUBLIC.MEMORY_MCP FROM SPECIFICATION $$
tools:
  - name: "search_memory"
    identifier: "<app>.API.SEARCH"
    type: "GENERIC"
    description: "Full-text search over the team memory's facts; JSON with entity, value, quote and source episode ids."
    config: {type: "function", warehouse: "<warehouse>", query_timeout: 60,
             input_schema: {type: "object", properties: {q: {type: "string"}, k: {type: "integer"}}}}
  - name: "get_episode"
    identifier: "<app>.API.EPISODE"
    type: "GENERIC"
    description: "The source text a fact cites, at most max_chars characters, centred on `around` (the fact's quote)."
    config: {type: "function", warehouse: "<warehouse>", query_timeout: 60,
             input_schema: {type: "object", properties: {episode_id: {type: "string"}, around: {type: "string"}, max_chars: {type: "integer"}}}}
  - name: "ask_memory"
    identifier: "<app>.CORE.ASK"
    type: "GENERIC"
    description: "Answer a question from the memory with Cortex, citing the source episodes."
    config: {type: "procedure", warehouse: "<warehouse>", query_timeout: 120,
             input_schema: {type: "object", properties: {q: {type: "string"}, model: {type: "string"}}}}
$$;
GRANT USAGE ON MCP SERVER MEMORY_DATA.PUBLIC.MEMORY_MCP TO ROLE MEMORY_USER;
```

Give each user `MEMORY_USER` as default role and a default warehouse. The endpoint is
`https://<org>-<account>.snowflakecomputing.com/api/v2/databases/MEMORY_DATA/schemas/PUBLIC/mcp-servers/MEMORY_MCP`.

- **Claude, Claude Cowork:** create an OAuth integration, then in Claude add a custom connector
  (Settings > Connectors) with the endpoint and the client id and secret:

  ```sql
  CREATE SECURITY INTEGRATION MEMORY_CLAUDE TYPE = OAUTH OAUTH_CLIENT = CUSTOM OAUTH_CLIENT_TYPE = 'PUBLIC'
    ENABLED = TRUE OAUTH_REDIRECT_URI = 'https://claude.ai/api/mcp/auth_callback'
    OAUTH_USE_SECONDARY_ROLES = NONE ALLOWED_ROLES_LIST = ('MEMORY_USER');
  SELECT SYSTEM$SHOW_OAUTH_CLIENT_SECRETS('MEMORY_CLAUDE');
  ```

- **Cortex Code:** with a personal access token for a `MEMORY_USER` user in `SNOWFLAKE_PAT`:
  `cortex mcp add memory <endpoint> --type http -H "Authorization: Bearer ${SNOWFLAKE_PAT}"`

The app also serves the same tools on its own endpoint (`CALL <app>.core.mcp_url();`) for clients that send a
Snowflake token header directly.

Analysts can read the memory directly: grant `USAGE` on `MEMORY_DATA`, plus `USAGE ON FUTURE SCHEMAS`
and `SELECT ON FUTURE ICEBERG TABLES` in it.

## Functionality

| Object | What it does |
|---|---|
| `core.remember(source, body, occurred_at[, acl])` | Queues text in the app's inbox table, readable by the entries in `acl` |
| `core.ingest(model)` | Extracts facts from queued text with `SNOWFLAKE.CORTEX.COMPLETE(model, ...)` and commits episodes + facts to `MEMORY_DATA` (a job in the app's compute pool) |
| `core.ask(question, model[, use_sources])` | Retrieves the best-matching facts and, unless `use_sources` is FALSE, excerpts of the episodes they cite (up to 5, ~4k characters each, centred on each fact's quote); Cortex answers from them and cites the episodes |
| `api.search(query, k)` | Full-text search over facts (service function to the engine) |
| `api.episode(episode_id, around, max_chars)` | An episode's text, at most `max_chars`, centred on `around`; NULL if it doesn't exist or isn't visible |
| `core.provision`, `core.engine_status`, `core.engine_logs`, `core.stop`, `core.resume`, `core.schedule_ingest`, `core.unschedule_ingest`, `core.backfill_quotes` | Operations (`APP_ADMIN`) |
| MCP endpoint (`core.mcp_url()`) | `search_memory`, `get_episode` for AI agents, behind Snowflake login (`APP_USER`) |

## Data, endpoints and AI disclosures

- **Your data stays in your account.** The app stores nothing outside it and sends nothing to the
  provider. It reads only text you pass to `core.remember` and the tables it writes in `MEMORY_DATA`.
- **AI:** Cortex models run in your account, billed to you, on the text you remember and the questions
  you ask. Your data is **not** used to train or tune any model.
- **What is stored:** episodes (the text you remember), extracted facts and a commit ledger, as Iceberg
  tables in `MEMORY_DATA`; queued text in the app's inbox until ingested. Questions and answers from
  `core.ask` are not stored.
- **Logs:** the engine's service logs (in your account) contain startup lines and error messages; they
  don't contain remembered text, questions or answers.
- **Endpoints:** one endpoint (MCP and `api.search`), reachable only after Snowflake authentication and
  only for roles granted `APP_USER`; requests carrying a browser Origin are refused. Outbound connections (HTTPS only), all to your own account and approved by you via the
  STORAGE_ACCESS specification: your account's Snowflake-managed Iceberg storage bucket and your
  account URL (`<org>-<account>.snowflakecomputing.com`, for the Iceberg catalog).
- **External functions:** none.
- **Cookies:** none (no web UI).

## Costs

One CPU_X64_S compute pool node while the engine runs (`core.stop()` to pause), Cortex tokens for
`ingest` and `ask`, a warehouse for the SQL procedures, and Iceberg storage in `MEMORY_DATA`.
