-- Memory: a governed team memory inside your Snowflake account.
-- Episodes (raw text) and facts (extracted with Cortex) live in Iceberg tables the app owns;
-- a memstore engine in Snowpark Container Services serves search; Cortex answers from it.

CREATE APPLICATION ROLE IF NOT EXISTS app_user;   -- ask/search/remember
CREATE APPLICATION ROLE IF NOT EXISTS app_admin;  -- provision, ingest, logs, credentials
GRANT APPLICATION ROLE app_user TO APPLICATION ROLE app_admin;

CREATE OR ALTER VERSIONED SCHEMA core;
GRANT USAGE ON SCHEMA core TO APPLICATION ROLE app_user;
GRANT USAGE ON SCHEMA core TO APPLICATION ROLE app_admin;

-- Not versioned: survives upgrades.
CREATE SCHEMA IF NOT EXISTS state;
CREATE SCHEMA IF NOT EXISTS services;
GRANT USAGE ON SCHEMA services TO APPLICATION ROLE app_user;  -- ingress to the MCP endpoint
CREATE SCHEMA IF NOT EXISTS api;
GRANT USAGE ON SCHEMA api TO APPLICATION ROLE app_user;

CREATE TABLE IF NOT EXISTS state.inbox (
  source STRING, body STRING, occurred_at TIMESTAMP_TZ,
  added_at TIMESTAMP_TZ DEFAULT CURRENT_TIMESTAMP()
);
CREATE STAGE IF NOT EXISTS state.batches ENCRYPTION = (TYPE = 'SNOWFLAKE_SSE');

-- Every episode is visible to this group, and every caller of the API searches as it.
-- ponytail: one org-wide audience; per-role ACLs when consumers need them.
CREATE OR REPLACE FUNCTION core.audience() RETURNS STRING AS $$ 'members' $$;

-- The engine's catalog credential: a PAT of a consumer service user whose role owns the memory
-- database (Horizon's Iceberg REST catalog doesn't accept the app's own identity).
CREATE OR REPLACE PROCEDURE core.register_reference(ref_name STRING, operation STRING, ref_or_alias STRING)
RETURNS STRING LANGUAGE SQL AS
$$
BEGIN
  CASE (operation)
    WHEN 'ADD' THEN SELECT SYSTEM$SET_REFERENCE(:ref_name, :ref_or_alias);
    WHEN 'REMOVE' THEN SELECT SYSTEM$REMOVE_REFERENCE(:ref_name, :ref_or_alias);
    WHEN 'CLEAR' THEN SELECT SYSTEM$REMOVE_ALL_REFERENCES(:ref_name);
    ELSE RETURN 'unknown operation: ' || operation;
  END CASE;
  RETURN NULL;
END
$$;
GRANT USAGE ON PROCEDURE core.register_reference(STRING, STRING, STRING) TO APPLICATION ROLE app_admin;

-- Tells Snowsight what kind of secret ENGINE_CREDENTIAL is.
CREATE OR REPLACE PROCEDURE core.reference_config(ref_name STRING)
RETURNS STRING LANGUAGE SQL AS
$$
BEGIN
  IF (UPPER(ref_name) = 'ENGINE_CREDENTIAL') THEN
    RETURN '{"type": "CONFIGURATION", "payload": {"type": "GENERIC_STRING"}}';
  END IF;
  RETURN '{"type": "ERROR", "payload": {"message": "unknown reference"}}';
END
$$;
GRANT USAGE ON PROCEDURE core.reference_config(STRING) TO APPLICATION ROLE app_admin;

CREATE TABLE IF NOT EXISTS state.config (database STRING, engine_role STRING);
-- A random value only the app's own wrappers and the engine know, proving an internal call came from them.
ALTER TABLE state.config ADD COLUMN IF NOT EXISTS fn_secret STRING;
-- Left over from a diagnostic in an earlier dev patch.
DROP TABLE IF EXISTS state.diag_roles;

-- Provision (and re-provision after upgrades): storage egress, compute pool, engine.
--   database:    a database you created with CATALOG = 'SNOWFLAKE', EXTERNAL_VOLUME = 'SNOWFLAKE_MANAGED'
--   engine_role: the role (owning that database) of the service user whose PAT is bound as ENGINE_CREDENTIAL
CREATE OR REPLACE PROCEDURE core.provision(database STRING, engine_role STRING)
RETURNS STRING LANGUAGE SQL EXECUTE AS OWNER AS
$$
DECLARE
  probe STRING DEFAULT CURRENT_DATABASE() || '_PROBE';
  pool STRING DEFAULT CURRENT_DATABASE() || '_POOL';
  eai STRING DEFAULT CURRENT_DATABASE() || '_STORAGE';
  using_clause STRING;
  account STRING;
  info VARIANT;
  bucket STRING;
  region STRING;
  hosts STRING;
  secret STRING;
BEGIN
  IF (NOT REGEXP_LIKE(database, '[A-Za-z_][A-Za-z0-9_$]*') OR NOT REGEXP_LIKE(engine_role, '[A-Za-z_][A-Za-z0-9_$]*')) THEN
    RETURN 'database and engine_role must be plain identifiers';
  END IF;
  secret := (SELECT MAX(fn_secret) FROM state.config);
  IF (secret IS NULL) THEN
    secret := REPLACE(UUID_STRING() || UUID_STRING(), '-', '');
  END IF;
  DELETE FROM state.config;
  INSERT INTO state.config (database, engine_role, fn_secret) VALUES (UPPER(:database), UPPER(:engine_role), :secret);
  account := LOWER(CURRENT_ORGANIZATION_NAME() || '-' || CURRENT_ACCOUNT_NAME());
  using_clause := ' USING (db => ''"' || UPPER(database) || '"'', role => ''"' || UPPER(engine_role) || '"'', account => ''"' || account || '"'', groups => ''"' || core.audience() || '"'', fn_secret => ''"' || secret || '"'')';

  -- Snowflake-managed storage is one bucket per account: find it from a probe table the app owns,
  -- and allow the engine (which reads/writes Iceberg files with vended credentials) to reach it.
  EXECUTE IMMEDIATE 'CREATE DATABASE IF NOT EXISTS ' || probe || ' CATALOG = ''SNOWFLAKE'' EXTERNAL_VOLUME = ''SNOWFLAKE_MANAGED''';
  EXECUTE IMMEDIATE 'CREATE SCHEMA IF NOT EXISTS ' || probe || '.probe';
  EXECUTE IMMEDIATE 'CREATE ICEBERG TABLE IF NOT EXISTS ' || probe || '.probe.t (a INT) CATALOG = ''SNOWFLAKE'' EXTERNAL_VOLUME = ''SNOWFLAKE_MANAGED''';
  info := PARSE_JSON(SYSTEM$GET_ICEBERG_TABLE_INFORMATION(:probe || '.probe.t'));
  IF (NOT STARTSWITH(info:metadataLocation::STRING, 's3://')) THEN
    RETURN 'Unsupported: this version runs on Snowflake accounts hosted on AWS.';
  END IF;
  bucket := SPLIT_PART(SPLIT_PART(info:metadataLocation::STRING, '://', 2), '/', 1);
  region := LOWER(REPLACE(REPLACE(CURRENT_REGION(), 'AWS_', ''), '_', '-'));
  hosts := '''' || bucket || '.s3.' || region || '.amazonaws.com:443'', ''' || bucket || '.s3.amazonaws.com:443'', ''s3.' || region || '.amazonaws.com:443'', ''' || account || '.snowflakecomputing.com:443''';
  EXECUTE IMMEDIATE 'CREATE OR REPLACE NETWORK RULE state.storage_rule MODE = EGRESS TYPE = HOST_PORT VALUE_LIST = (' || hosts || ')';
  EXECUTE IMMEDIATE 'CREATE EXTERNAL ACCESS INTEGRATION IF NOT EXISTS ' || eai || ' ALLOWED_NETWORK_RULES = (state.storage_rule) ENABLED = TRUE';
  EXECUTE IMMEDIATE 'ALTER APPLICATION SET SPECIFICATION storage_access TYPE = EXTERNAL_ACCESS LABEL = ''Iceberg storage'' DESCRIPTION = ''Read and write the memory''''s Iceberg files in this account''''s Snowflake-managed storage, through this account''''s Iceberg catalog'' HOST_PORTS = (' || hosts || ')';

  EXECUTE IMMEDIATE 'CREATE COMPUTE POOL IF NOT EXISTS ' || pool || ' MIN_NODES = 1 MAX_NODES = 2 INSTANCE_FAMILY = CPU_X64_S AUTO_RESUME = TRUE AUTO_SUSPEND_SECS = 3600';
  EXECUTE IMMEDIATE 'CREATE SERVICE IF NOT EXISTS services.engine IN COMPUTE POOL ' || pool ||
    ' EXTERNAL_ACCESS_INTEGRATIONS = (' || eai || ') MIN_INSTANCES = 1 MAX_INSTANCES = 1' ||
    ' FROM SPECIFICATION_TEMPLATE_FILE = ''service/engine.yaml''' || using_clause;
  -- Upgrades ship new specs/images: always apply the current one.
  EXECUTE IMMEDIATE 'ALTER SERVICE services.engine FROM SPECIFICATION_TEMPLATE_FILE = ''service/engine.yaml''' || using_clause;
  -- Restart, so egress approved since the last start (STORAGE_ACCESS) takes effect.
  ALTER SERVICE services.engine SUSPEND;
  ALTER SERVICE services.engine RESUME;

  -- Per-user access. Service functions carry no caller identity, so the engine's service functions sit in a
  -- schema granted to no one, on the internal endpoint `fn` that no service role includes; only the app's own
  -- wrappers below call them, adding the caller: CURRENT_USER() and the ACL roles IS_ROLE_IN_SESSION confirms
  -- (inherited roles count), plus the app's secret. Users can't edit app code, so they can't change either.
  CREATE SCHEMA IF NOT EXISTS engine_fn;
  CREATE OR REPLACE FUNCTION engine_fn.search(q VARCHAR, k INT, valid_at INT, snippet_chars INT,
                                              secret VARCHAR, caller VARCHAR, roles ARRAY)
    RETURNS VARIANT SERVICE = services.engine ENDPOINT = fn AS '/search';
  CREATE OR REPLACE FUNCTION engine_fn.episode(episode_id VARCHAR, around VARCHAR, max_chars INT,
                                               secret VARCHAR, caller VARCHAR, roles ARRAY)
    RETURNS VARCHAR SERVICE = services.engine ENDPOINT = fn AS '/episode';
  -- Every role named in a committed episode's ACL, from the engine (the one source of truth).
  CREATE OR REPLACE FUNCTION engine_fn.acl_roles(secret VARCHAR)
    RETURNS VARIANT SERVICE = services.engine ENDPOINT = fn AS '/acl_roles';
  -- (function bodies in single quotes: they sit inside this procedure's body)
  CREATE OR REPLACE FUNCTION engine_fn.secret() RETURNS VARCHAR AS '(SELECT MAX(fn_secret) FROM state.config)';
  CREATE OR REPLACE FUNCTION engine_fn.caller_roles() RETURNS ARRAY AS
  '(SELECT COALESCE(ARRAY_AGG(f.value::STRING), ARRAY_CONSTRUCT())
      FROM TABLE(FLATTEN(input => engine_fn.acl_roles(engine_fn.secret()))) f
      WHERE IS_ROLE_IN_SESSION(f.value::STRING))';

  -- Search as the caller. Same signature as before, so SQL users and MCP tools built on it keep working.
  -- (Earlier versions made these service functions, which CREATE OR REPLACE can't turn into SQL functions.)
  DROP FUNCTION IF EXISTS api.search(VARCHAR, INT);
  DROP FUNCTION IF EXISTS api.episode(VARCHAR, VARCHAR, INT);
  CREATE OR REPLACE FUNCTION api.search(q VARCHAR, k INT) RETURNS VARIANT AS
  'engine_fn.search(q, k, NULL, NULL, engine_fn.secret(), CURRENT_USER(), engine_fn.caller_roles())';
  GRANT USAGE ON FUNCTION api.search(VARCHAR, INT) TO APPLICATION ROLE app_user;
  -- An episode's text (null if missing or not visible to the caller), at most max_chars, centred on `around`.
  CREATE OR REPLACE FUNCTION api.episode(episode_id VARCHAR, around VARCHAR, max_chars INT) RETURNS VARCHAR AS
  'engine_fn.episode(episode_id, around, max_chars, engine_fn.secret(), CURRENT_USER(), engine_fn.caller_roles())';
  GRANT USAGE ON FUNCTION api.episode(VARCHAR, VARCHAR, INT) TO APPLICATION ROLE app_user;
  GRANT SERVICE ROLE services.engine!memory TO APPLICATION ROLE app_user;
  RETURN 'provisioned: memory in ' || UPPER(database) || ', engine in ' || pool ||
    '. First run: approve the STORAGE_ACCESS app specification, then CALL core.provision(...) once more. Then CALL core.engine_status() until READY.';
END
$$;
GRANT USAGE ON PROCEDURE core.provision(STRING, STRING) TO APPLICATION ROLE app_admin;

CREATE OR REPLACE PROCEDURE core.engine_status()
RETURNS VARIANT LANGUAGE SQL EXECUTE AS OWNER AS
$$
BEGIN
  RETURN PARSE_JSON(SYSTEM$GET_SERVICE_STATUS('services.engine'));
END
$$;
GRANT USAGE ON PROCEDURE core.engine_status() TO APPLICATION ROLE app_admin;

-- Queue text for the memory: a Slack thread, a PR, a call transcript, a doc.
CREATE OR REPLACE PROCEDURE core.remember(source STRING, body STRING, occurred_at TIMESTAMP_TZ)
RETURNS STRING LANGUAGE SQL EXECUTE AS OWNER AS
$$
BEGIN
  INSERT INTO state.inbox (source, body, occurred_at) VALUES (:source, :body, :occurred_at);
  RETURN 'queued';
END
$$;
GRANT USAGE ON PROCEDURE core.remember(STRING, STRING, TIMESTAMP_TZ) TO APPLICATION ROLE app_user;
-- The same, visible only to the readers acl names: entries `user:<NAME>`, `role:<ROLE>` (a Snowflake role; anyone
-- holding it, directly or by inheritance) or `*`. Without an acl, everyone the app is shared with may read it.
ALTER TABLE state.inbox ADD COLUMN IF NOT EXISTS acl ARRAY;
CREATE OR REPLACE PROCEDURE core.remember(source STRING, body STRING, occurred_at TIMESTAMP_TZ, acl ARRAY)
RETURNS STRING LANGUAGE SQL EXECUTE AS OWNER AS
$$
BEGIN
  INSERT INTO state.inbox (source, body, occurred_at, acl) SELECT :source, :body, :occurred_at, :acl;
  RETURN 'queued';
END
$$;
GRANT USAGE ON PROCEDURE core.remember(STRING, STRING, TIMESTAMP_TZ, ARRAY) TO APPLICATION ROLE app_user;

-- Extract facts from everything queued (Cortex, in this account) and commit episodes + facts.
CREATE OR REPLACE PROCEDURE core.ingest(model STRING)
RETURNS STRING LANGUAGE SQL EXECUTE AS OWNER AS
$$
DECLARE
  db STRING;
  role STRING;
  pool STRING DEFAULT CURRENT_DATABASE() || '_POOL';
  eai STRING DEFAULT CURRENT_DATABASE() || '_STORAGE';
  batch STRING;
  -- Per-run tables: concurrent ingests never see (or delete) each other's rows.
  bt STRING DEFAULT 'state.batch_' || TO_VARCHAR(CURRENT_TIMESTAMP(), 'YYYYMMDDHH24MISSFF3') || '_' || UNIFORM(0, 999999, RANDOM());
  ft STRING DEFAULT bt || '_facts';
  n_eps INT;
  n_facts INT;
BEGIN
  SELECT database, engine_role INTO :db, :role FROM state.config;
  IF (db IS NULL) THEN RETURN 'not provisioned: CALL core.provision(<database>, <engine_role>) first'; END IF;
  batch := SPLIT_PART(bt, '.', 2);
  CREATE TRANSIENT TABLE IDENTIFIER(:bt) AS
    SELECT LOWER(SHA2(body, 256)) AS eid, ANY_VALUE(source) AS source, ANY_VALUE(body) AS body,
           DATE_PART(EPOCH_MICROSECOND, COALESCE(MIN(occurred_at), MIN(added_at))) AS occurred_us,
           MAX(added_at) AS added_at,
           -- readers: the union of the valid entries it was queued with (invalid ones dropped)
           -- user:/role: names are Snowflake identifiers: stored upper-case, as Snowflake stores unquoted ones
           TRANSFORM(FILTER(ARRAY_UNION_AGG(acl), a -> REGEXP_LIKE(a::STRING, '[*]|(user|role|group):[A-Za-z0-9_.@$-]+')),
                     a -> IFF(REGEXP_LIKE(a::STRING, '(user|role):.*'),
                              SPLIT_PART(a::STRING, ':', 1) || ':' || UPPER(SUBSTR(a::STRING, POSITION(':', a::STRING) + 1)),
                              a::STRING)) AS acl
    FROM state.inbox GROUP BY 1;
  SELECT COUNT(*) INTO :n_eps FROM IDENTIFIER(:bt);
  IF (n_eps = 0) THEN
    DROP TABLE IDENTIFIER(:bt);
    RETURN 'nothing queued';
  END IF;

  CREATE TRANSIENT TABLE IDENTIFIER(:ft) AS
  WITH replies AS (
    SELECT eid, body, occurred_us,
      TRY_PARSE_JSON(REGEXP_SUBSTR(REGEXP_REPLACE(SNOWFLAKE.CORTEX.COMPLETE(:model,
        'Extract durable facts a teammate would want remembered from this text.\n\nKinds:\n' ||
        '- decision: something decided or proposed, with status\n- issue: a problem, bug or risk\n' ||
        '- owner: who owns or is doing something\n- how_it_works: how a feature or system behaves\n' ||
        '- how_to_enable: how to turn something on or configure it (exact flag/setting)\n' ||
        '- supports: whether something is supported (yes/no + detail)\n- shipped_in / changed_in: where and when something shipped or changed\n' ||
        '- requested_by: who asked for something\n\nRules:\n- Only facts the text states outright. No guesses.\n' ||
        '- quote = an exact, verbatim span of the text (under 300 chars) supporting the fact.\n' ||
        '- feature = the short name of the thing the fact is about.\n\n' ||
        'Reply with JSON only: {"rows": [{"kind": str, "feature": str, "value": str, "quote": str}]}. ' ||
        'Return {"rows": []} when there is nothing.\n\nText:\n' || body),
        '<think>[\\s\\S]*?</think>', ''), '\\{[\\s\\S]*\\}')) AS j
    FROM IDENTIFIER(:bt)
  )
  SELECT UUID_STRING() AS fact_id, LOWER(TRIM(r.value:feature::STRING)) AS entity,
         r.value:kind::STRING AS predicate,
         '(' || r.value:kind::STRING || ') ' || r.value:value::STRING AS value,
         TRIM(r.value:quote::STRING) AS quote, occurred_us, eid
  FROM replies, LATERAL FLATTEN(input => j:rows) r
  WHERE r.value:kind::STRING IN ('decision','issue','owner','how_it_works','how_to_enable','supports','shipped_in','changed_in','requested_by')
    AND NULLIF(TRIM(r.value:feature::STRING), '') IS NOT NULL
    -- Grounding: a real quote (an empty one is "contained" in anything) found in the text.
    AND LENGTH(TRIM(REGEXP_REPLACE(LOWER(r.value:quote::STRING), '[^a-z0-9]+', ' '))) >= 12
    AND CONTAINS(REGEXP_REPLACE(LOWER(body), '[^a-z0-9]+', ' '),
                 TRIM(REGEXP_REPLACE(LOWER(r.value:quote::STRING), '[^a-z0-9]+', ' ')));
  SELECT COUNT(*) INTO :n_facts FROM IDENTIFIER(:ft);

  EXECUTE IMMEDIATE 'COPY INTO @state.batches/' || batch || '/episodes.jsonl FROM (SELECT OBJECT_CONSTRUCT(' ||
    '''episode_id'', eid, ''source'', source, ''occurred_at'', occurred_us, ''scope'', ''org'', ' ||
    '''acl'', IFF(ARRAY_SIZE(acl) > 0, acl, ARRAY_CONSTRUCT(''group:' || core.audience() || ''')), ''body'', body) FROM ' || bt || ') ' ||
    'FILE_FORMAT = (TYPE = JSON COMPRESSION = NONE) SINGLE = TRUE OVERWRITE = TRUE MAX_FILE_SIZE = 5368709120';
  EXECUTE IMMEDIATE 'COPY INTO @state.batches/' || batch || '/facts.jsonl FROM (SELECT OBJECT_CONSTRUCT(' ||
    '''fact_id'', fact_id, ''scope'', ''org'', ''entity'', entity, ''predicate'', predicate, ''value'', value, ' ||
    '''valid_from'', occurred_us, ''quote'', quote, ''source_episodes'', ARRAY_CONSTRUCT(eid)) FROM ' || ft || ') ' ||
    'FILE_FORMAT = (TYPE = JSON COMPRESSION = NONE) SINGLE = TRUE OVERWRITE = TRUE MAX_FILE_SIZE = 5368709120';

  BEGIN
    EXECUTE IMMEDIATE 'EXECUTE JOB SERVICE IN COMPUTE POOL ' || pool || ' NAME = services.load_' || batch ||
      ' EXTERNAL_ACCESS_INTEGRATIONS = (' || eai || ')' ||
      ' FROM SPECIFICATION_TEMPLATE_FILE = ''service/load.yaml'' USING (db => ''"' || db || '"'', role => ''"' || role || '"'', account => ''"' || LOWER(CURRENT_ORGANIZATION_NAME() || '-' || CURRENT_ACCOUNT_NAME()) || '"'', path => ''"/batches/' || batch || '"'')';
  EXCEPTION
    WHEN OTHER THEN
      -- The inbox is untouched; report the error and, if the job ran, its own output.
      LET logs STRING := SQLERRM;
      BEGIN
        logs := logs || '\n' || SYSTEM$GET_SERVICE_LOGS('services.load_' || batch, 0, 'main', 40);
      EXCEPTION
        WHEN OTHER THEN NULL;
      END;
      EXECUTE IMMEDIATE 'DROP SERVICE IF EXISTS services.load_' || batch;
      EXECUTE IMMEDIATE 'DROP TABLE IF EXISTS ' || bt;
      EXECUTE IMMEDIATE 'DROP TABLE IF EXISTS ' || ft;
      RETURN 'load failed (inbox kept): ' || logs;
  END;

  -- Committed (a failed job raises above, keeping the inbox): drop only what this run covered.
  DELETE FROM state.inbox i USING IDENTIFIER(:bt) b
    WHERE LOWER(SHA2(i.body, 256)) = b.eid AND i.added_at <= b.added_at;
  DROP TABLE IDENTIFIER(:bt);
  DROP TABLE IDENTIFIER(:ft);
  EXECUTE IMMEDIATE 'REMOVE @state.batches/' || batch || '/';
  EXECUTE IMMEDIATE 'DROP SERVICE IF EXISTS services.load_' || batch;
  RETURN 'remembered ' || n_eps || ' episodes, ' || n_facts || ' facts';
END
$$;
GRANT USAGE ON PROCEDURE core.ingest(STRING) TO APPLICATION ROLE app_admin;

-- Attach a supporting quote to facts written without one (the sentence of a cited episode that
-- best matches the fact), so answers can centre episode excerpts on it. Safe to re-run; with
-- redo, every backfilled quote is recomputed (extraction's own quotes are never replaced).
CREATE OR REPLACE PROCEDURE core.backfill_quotes(redo BOOLEAN)
RETURNS STRING LANGUAGE SQL EXECUTE AS OWNER AS
$$
DECLARE
  db STRING;
  role STRING;
  pool STRING DEFAULT CURRENT_DATABASE() || '_POOL';
  eai STRING DEFAULT CURRENT_DATABASE() || '_STORAGE';
  job STRING DEFAULT 'services.backfill_' || TO_VARCHAR(CURRENT_TIMESTAMP(), 'YYYYMMDDHH24MISSFF3');
  logs STRING;
BEGIN
  SELECT database, engine_role INTO :db, :role FROM state.config;
  IF (db IS NULL) THEN RETURN 'not provisioned: CALL core.provision(<database>, <engine_role>) first'; END IF;
  BEGIN
    EXECUTE IMMEDIATE 'EXECUTE JOB SERVICE IN COMPUTE POOL ' || pool || ' NAME = ' || job ||
      ' EXTERNAL_ACCESS_INTEGRATIONS = (' || eai || ')' ||
      ' FROM SPECIFICATION_TEMPLATE_FILE = ''service/backfill.yaml'' USING (db => ''"' || db || '"'', role => ''"' || role || '"'', account => ''"' || LOWER(CURRENT_ORGANIZATION_NAME() || '-' || CURRENT_ACCOUNT_NAME()) || '"'', redo => ''"' || IFF(redo, 'true', 'false') || '"'')';
  EXCEPTION
    WHEN OTHER THEN
      logs := SQLERRM;
  END;
  BEGIN
    logs := COALESCE(logs || '\n', '') || SYSTEM$GET_SERVICE_LOGS(job, 0, 'main', 5);
  EXCEPTION
    WHEN OTHER THEN NULL;
  END;
  EXECUTE IMMEDIATE 'DROP SERVICE IF EXISTS ' || job;
  RETURN logs;
END
$$;
GRANT USAGE ON PROCEDURE core.backfill_quotes(BOOLEAN) TO APPLICATION ROLE app_admin;

CREATE OR REPLACE PROCEDURE core.backfill_quotes()
RETURNS STRING LANGUAGE SQL EXECUTE AS OWNER AS
$$
DECLARE
  out STRING;
BEGIN
  CALL core.backfill_quotes(FALSE) INTO :out;
  RETURN out;
END
$$;
GRANT USAGE ON PROCEDURE core.backfill_quotes() TO APPLICATION ROLE app_admin;

-- Ask the memory: retrieve the best-matching facts and answer from them with Cortex.
-- Answer a question from the memory. With use_sources (the default), the answer also reads
-- excerpts of the episodes the top facts cite (up to 5, ~4k characters each, centred on each
-- fact's quote), so it can explain why and fill in what a one-line fact leaves out.
-- The answer step of core.ask: Cortex over facts and excerpts the caller was already allowed to read. It reads
-- no memory itself, so it runs with the app's rights (Cortex isn't visible to restricted-caller code).
CREATE OR REPLACE PROCEDURE core.answer(q STRING, model STRING, facts STRING, sources STRING)
RETURNS STRING LANGUAGE SQL EXECUTE AS OWNER AS
$$
BEGIN
  RETURN (SELECT TRIM(REGEXP_REPLACE(SNOWFLAKE.CORTEX.COMPLETE(:model,
    'Answer the question from the team memory below: facts (JSON, best match first) and, when present, ' ||
    'excerpts of the source episodes the facts cite. Use the excerpts for detail and reasons; cite them ' ||
    'as [episode <id>]. Be concise and name the people, features and dates involved. ' ||
    'If the memory does not answer it, say "unknown".' ||
    '\n\nFacts: ' || :facts ||
    IFF(COALESCE(:sources, '') = '', '', '\n\nSource excerpts:\n' || :sources) ||
    '\n\nQuestion: ' || :q), '<think>[\\s\\S]*?</think>', '')));
END
$$;
GRANT USAGE ON PROCEDURE core.answer(STRING, STRING, STRING, STRING) TO APPLICATION ROLE app_user;

-- Runs as the caller (restricted caller's rights), so api.search and api.episode see the caller's roles.
CREATE OR REPLACE PROCEDURE core.ask(q STRING, model STRING, use_sources BOOLEAN)
RETURNS STRING LANGUAGE SQL EXECUTE AS RESTRICTED CALLER AS
$$
DECLARE
  facts STRING;
  sources STRING DEFAULT '';
  answer STRING;
BEGIN
  SELECT TO_VARCHAR(api.search(:q, 15)) INTO :facts;
  IF (use_sources) THEN
    -- Concatenated in a loop: an ordered LISTAGG/ARRAY_AGG over service-function results fails
    -- with a Snowflake internal error, while a plain ORDER BY works.
    LET rs RESULTSET := (
      WITH cited AS (
        SELECT e.value::STRING AS eid, MIN(h.index) AS rnk, MIN_BY(h.value:quote::STRING, h.index) AS quote
          FROM TABLE(FLATTEN(input => PARSE_JSON(:facts))) h,
               TABLE(FLATTEN(input => h.value:source_episodes)) e
         GROUP BY 1 ORDER BY rnk LIMIT 5)
      SELECT eid, api.episode(eid, quote, 4000) AS txt FROM cited ORDER BY rnk);
    FOR r IN rs DO
      IF (r.txt IS NOT NULL) THEN
        sources := sources || '[episode ' || LEFT(r.eid, 12) || ']\n' || r.txt || '\n\n';
      END IF;
    END FOR;
  END IF;
  -- Cortex runs in core.answer (the app's own rights): only the text gathered above, as the caller, goes in.
  CALL core.answer(:q, :model, :facts, :sources) INTO :answer;
  RETURN answer;
END
$$;
GRANT USAGE ON PROCEDURE core.ask(STRING, STRING, BOOLEAN) TO APPLICATION ROLE app_user;

CREATE OR REPLACE PROCEDURE core.ask(q STRING, model STRING)
RETURNS STRING LANGUAGE SQL EXECUTE AS RESTRICTED CALLER AS
$$
DECLARE
  answer STRING;
BEGIN
  CALL core.ask(:q, :model, TRUE) INTO :answer;
  RETURN answer;
END
$$;
GRANT USAGE ON PROCEDURE core.ask(STRING, STRING) TO APPLICATION ROLE app_user;

-- Recent engine logs, for troubleshooting (share with support if asked).
CREATE OR REPLACE PROCEDURE core.engine_logs(lines INT)
RETURNS STRING LANGUAGE SQL EXECUTE AS OWNER AS
$$
BEGIN
  RETURN SYSTEM$GET_SERVICE_LOGS('services.engine', 0, 'main', :lines);
END
$$;
GRANT USAGE ON PROCEDURE core.engine_logs(INT) TO APPLICATION ROLE app_admin;


-- Pause the engine (and its compute pool) when the memory isn't needed; resume brings it back.
CREATE OR REPLACE PROCEDURE core.stop()
RETURNS STRING LANGUAGE SQL EXECUTE AS OWNER AS
$$
BEGIN
  ALTER SERVICE IF EXISTS services.engine SUSPEND;
  EXECUTE IMMEDIATE 'ALTER COMPUTE POOL IF EXISTS ' || CURRENT_DATABASE() || '_POOL SUSPEND';
  RETURN 'stopped: no compute is billed until CALL core.resume()';
END
$$;
GRANT USAGE ON PROCEDURE core.stop() TO APPLICATION ROLE app_admin;

CREATE OR REPLACE PROCEDURE core.resume()
RETURNS STRING LANGUAGE SQL EXECUTE AS OWNER AS
$$
BEGIN
  EXECUTE IMMEDIATE 'ALTER COMPUTE POOL IF EXISTS ' || CURRENT_DATABASE() || '_POOL RESUME';
  ALTER SERVICE IF EXISTS services.engine RESUME;
  RETURN 'resuming: CALL core.engine_status() until READY';
END
$$;
GRANT USAGE ON PROCEDURE core.resume() TO APPLICATION ROLE app_admin;

-- Scheduled ingestion: a serverless task that runs core.ingest(model) every `minutes`.
CREATE OR REPLACE PROCEDURE core.schedule_ingest(model STRING, minutes INT)
RETURNS STRING LANGUAGE SQL EXECUTE AS OWNER AS
$$
BEGIN
  IF (NOT REGEXP_LIKE(model, '[a-z0-9][a-z0-9.-]*') OR minutes < 5 OR minutes > 1440) THEN
    RETURN 'model must be a Cortex model name, minutes between 5 and 1440';
  END IF;
  EXECUTE IMMEDIATE 'CREATE OR REPLACE TASK state.ingest_task SCHEDULE = ''' || minutes || ' MINUTE''' ||
    ' USER_TASK_MANAGED_INITIAL_WAREHOUSE_SIZE = ''XSMALL'' AS CALL core.ingest(''' || model || ''')';
  ALTER TASK state.ingest_task RESUME;
  RETURN 'ingesting with ' || model || ' every ' || minutes || ' minutes';
END
$$;
GRANT USAGE ON PROCEDURE core.schedule_ingest(STRING, INT) TO APPLICATION ROLE app_admin;

CREATE OR REPLACE PROCEDURE core.unschedule_ingest()
RETURNS STRING LANGUAGE SQL EXECUTE AS OWNER AS
$$
BEGIN
  DROP TASK IF EXISTS state.ingest_task;
  RETURN 'scheduled ingestion stopped';
END
$$;
GRANT USAGE ON PROCEDURE core.unschedule_ingest() TO APPLICATION ROLE app_admin;

-- Where MCP clients connect (Snowflake login required; APP_USER only).
CREATE OR REPLACE PROCEDURE core.mcp_url()
RETURNS STRING LANGUAGE SQL EXECUTE AS OWNER AS
$$
DECLARE
  url STRING;
BEGIN
  SHOW ENDPOINTS IN SERVICE services.engine;
  SELECT "ingress_url" INTO :url FROM TABLE(RESULT_SCAN(LAST_QUERY_ID())) WHERE "name" = 'api';
  RETURN 'https://' || url || '/mcp';
END
$$;
GRANT USAGE ON PROCEDURE core.mcp_url() TO APPLICATION ROLE app_user;
