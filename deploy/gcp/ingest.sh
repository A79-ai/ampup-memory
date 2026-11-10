#!/usr/bin/env bash
# Extract facts from everything queued in memstore_api.inbox with Gemini (BigQuery AI.GENERATE),
# export the batch to the bucket and commit it with the memstore-load Cloud Run job. GCP's
# counterpart of the Snowflake app's core.ingest. Queue text with:
#   INSERT memstore_api.inbox (source, body, occurred_at, acl)
#     VALUES ('slack:#eng', '...', CURRENT_TIMESTAMP(), ['group:eng'])
# Each episode is visible to the union of the ACLs it was queued with (entries: *, user:<id>,
# group:<id>; others are dropped). A row with no ACL uses MEMSTORE_DEFAULT_ACL (e.g. group:team) if
# set, and is skipped otherwise: nothing becomes visible to an audience nobody chose.
#   GCP_PROJECT=<project> GCS_BUCKET=<bucket> deploy/gcp/ingest.sh [gemini model]
# The ingest job that setup.sh and terraform/ create sets GCP_PROJECT and GCS_BUCKET (both required) for it.
set -euo pipefail
P=${GCP_PROJECT:?set GCP_PROJECT to the project of the memory}
R=${GCP_REGION:-us-central1}
B=${GCS_BUCKET:?set GCS_BUCKET to the bucket of the memory}
MODEL=${1:-gemini-2.5-flash}
DEFAULT_ACL=${MEMSTORE_DEFAULT_ACL:-}
DS=${MEMSTORE_DATASET:-memstore_api}            # several installs can share a project
CONN=${MEMSTORE_CONNECTION:-$R.memstore}
LOAD_JOB=${MEMSTORE_LOAD_JOB:-memstore-load}
[[ $DS =~ ^[A-Za-z0-9_]+$ && $CONN =~ ^[a-z0-9-]+\.[A-Za-z0-9_-]+$ && $LOAD_JOB =~ ^[a-z0-9-]+$ ]] || { echo "bad dataset, connection or job name"; exit 1; }
[[ -z $DEFAULT_ACL || $DEFAULT_ACL =~ ^(\*|(user|group):[A-Za-z0-9_.@-]+)$ ]] || { echo "bad MEMSTORE_DEFAULT_ACL"; exit 1; }
[[ $MODEL =~ ^[a-z0-9][a-z0-9.-]*$ ]] || { echo "bad model name"; exit 1; }
CUTOFF=$(date -u +%Y-%m-%dT%H:%M:%S)
BATCH=batch_$(date -u +%Y%m%d%H%M%S)_$RANDOM
q() { bq query --project_id "$P" --location "$R" --use_legacy_sql=false --quiet "$@"; }

q "CREATE TABLE IF NOT EXISTS $DS.inbox (source STRING, body STRING, occurred_at TIMESTAMP,
     added_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP(), acl ARRAY<STRING>)"
q "ALTER TABLE $DS.inbox ADD COLUMN IF NOT EXISTS acl ARRAY<STRING>"
n=$(q --format=csv "SELECT COUNT(*) FROM $DS.inbox WHERE added_at <= '$CUTOFF'" | tail -1)
[ "$n" = 0 ] && { echo "nothing queued"; exit 0; }

q "
CREATE TEMP TABLE batch AS
WITH queued AS (
  SELECT TO_HEX(SHA256(body)) AS eid, source, body, occurred_at, added_at,
         IF(ARRAY_LENGTH(acl) > 0, acl, IF('$DEFAULT_ACL' = '', [], ['$DEFAULT_ACL'])) AS eff
  FROM $DS.inbox WHERE added_at <= '$CUTOFF'),
acls AS (  -- union of valid entries per episode; episodes with none are left out (fail closed)
  SELECT eid, ARRAY_AGG(DISTINCT a) AS acl FROM queued, UNNEST(eff) AS a
  WHERE REGEXP_CONTAINS(a, r'^(\*|(user|group):[A-Za-z0-9_.@-]+)$') GROUP BY eid)
SELECT eid, ANY_VALUE(source) AS source, ANY_VALUE(body) AS body,
       UNIX_MICROS(COALESCE(MIN(occurred_at), MIN(added_at))) AS occ, ANY_VALUE(acls.acl) AS acl
FROM queued JOIN acls USING (eid) GROUP BY eid;

CREATE TEMP TABLE replies AS
SELECT eid, body, occ, AI.GENERATE(CONCAT(
  'Extract durable facts a teammate would want remembered from this text.\n\nKinds:\n',
  '- decision: something decided or proposed, with status\n- issue: a problem, bug or risk\n',
  '- owner: who owns or is doing something\n- how_it_works: how a feature or system behaves\n',
  '- how_to_enable: how to turn something on or configure it (exact flag/setting)\n',
  '- supports: whether something is supported (yes/no + detail)\n',
  '- shipped_in / changed_in: where and when something shipped or changed\n',
  '- requested_by: who asked for something\n\nRules:\n- Only facts the text states outright. No guesses.\n',
  '- quote = an exact, verbatim span of the text (under 300 chars) supporting the fact.\n',
  '- feature = the short name of the thing the fact is about.\n\n',
  'Reply with JSON only: {\"rows\": [{\"kind\": str, \"feature\": str, \"value\": str, \"quote\": str}]}. ',
  'Return {\"rows\": []} when there is nothing.\n\nText:\n', body),
  connection_id => '$CONN', endpoint => '$MODEL').result AS reply
FROM batch;

CREATE TEMP TABLE facts AS
SELECT GENERATE_UUID() AS fact_id, 'org' AS scope, LOWER(TRIM(JSON_VALUE(r, '\$.feature'))) AS entity,
       JSON_VALUE(r, '\$.kind') AS predicate,
       CONCAT('(', JSON_VALUE(r, '\$.kind'), ') ', JSON_VALUE(r, '\$.value')) AS value,
       TRIM(JSON_VALUE(r, '\$.quote')) AS quote,
       occ AS valid_from, [eid] AS source_episodes
FROM replies, UNNEST(JSON_QUERY_ARRAY(SAFE.PARSE_JSON(REGEXP_EXTRACT(reply, r'(?s)\{.*\}')), '\$.rows')) AS r
WHERE JSON_VALUE(r, '\$.kind') IN ('decision','issue','owner','how_it_works','how_to_enable','supports','shipped_in','changed_in','requested_by')
  AND NULLIF(TRIM(JSON_VALUE(r, '\$.feature')), '') IS NOT NULL
  -- Grounding: a real quote (an empty one would match anything) that occurs in the text.
  AND LENGTH(TRIM(REGEXP_REPLACE(LOWER(JSON_VALUE(r, '\$.quote')), r'[^a-z0-9]+', ' '))) >= 12
  AND STRPOS(REGEXP_REPLACE(LOWER(body), r'[^a-z0-9]+', ' '),
             TRIM(REGEXP_REPLACE(LOWER(JSON_VALUE(r, '\$.quote')), r'[^a-z0-9]+', ' '))) > 0;

EXPORT DATA OPTIONS (uri = 'gs://$B/_corpora/$BATCH/episodes-*.json', format = 'JSON', overwrite = true) AS
SELECT eid AS episode_id, source, occ AS occurred_at, 'org' AS scope, acl, body FROM batch;
EXPORT DATA OPTIONS (uri = 'gs://$B/_corpora/$BATCH/facts-*.json', format = 'JSON', overwrite = true) AS
SELECT * FROM facts;
SELECT (SELECT COUNT(*) FROM batch) AS episodes, (SELECT COUNT(*) FROM facts) AS facts;
"
gcloud run jobs execute "$LOAD_JOB" --project "$P" --region "$R" --args="load,/data/_corpora/$BATCH,1000" --wait --quiet
# Rows skipped for having no ACL stay queued (and visible here) until someone gives them one.
q "DELETE FROM $DS.inbox WHERE added_at <= '$CUTOFF' AND ARRAY_LENGTH(ARRAY(
     SELECT a FROM UNNEST(IF(ARRAY_LENGTH(acl) > 0, acl, IF('$DEFAULT_ACL' = '', [], ['$DEFAULT_ACL']))) AS a
     WHERE REGEXP_CONTAINS(a, r'^(\\*|(user|group):[A-Za-z0-9_.@-]+)$'))) > 0"
gcloud storage rm -r "gs://$B/_corpora/$BATCH" --quiet
echo "committed $BATCH"
