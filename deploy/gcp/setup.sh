#!/usr/bin/env bash
# The memory on GCP: BigLake Iceberg catalog on a private bucket, the engine on Cloud Run (a load
# job and a warm search service, as a dedicated service account), BigQuery remote function over
# the service, and Gemini answers via AI.GENERATE. Mirror of deploy/native (Snowflake).
#   GCP_PROJECT=<project> GCS_BUCKET=<bucket> deploy/gcp/setup.sh   # one-time setup; the image is built by deploy/gcp/README notes
#
#   GCP_PROJECT     the project to set it up in (required)
#   GCS_BUCKET      the memory's bucket, created if missing (required)
#   GCP_REGION      default us-central1
#   MEMSTORE_GROUP  the group every caller reads as (default: team); share a memory with it as group:<it>
set -euo pipefail
P=${GCP_PROJECT:?set GCP_PROJECT to the project to set memstore up in}
R=${GCP_REGION:-us-central1}
B=${GCS_BUCKET:?set GCS_BUCKET to the bucket for the memory (created if missing)}
G=${MEMSTORE_GROUP:-team}
[[ $G =~ ^[A-Za-z0-9_.@-]+$ ]] || { echo "bad MEMSTORE_GROUP"; exit 1; }
SA=memstore-engine@$P.iam.gserviceaccount.com
I=$R-docker.pkg.dev/$P/memstore/memstore:dev

gcloud services enable run.googleapis.com bigqueryconnection.googleapis.com aiplatform.googleapis.com --project "$P"
gcloud storage buckets create "gs://$B" --project "$P" --location "$R" --uniform-bucket-level-access --public-access-prevention || true
gcloud biglake iceberg catalogs create "$B" --project "$P" --catalog-type gcs-bucket --credential-mode end-user || true
gcloud artifacts repositories create memstore --project "$P" --location "$R" --repository-format docker || true

# The engine's own identity: objects in this bucket, and the BigLake catalog.
gcloud iam service-accounts create memstore-engine --project "$P" --display-name "memstore engine (Cloud Run)" || true
gcloud storage buckets add-iam-policy-binding "gs://$B" --member "serviceAccount:$SA" --role roles/storage.objectAdmin
gcloud projects add-iam-policy-binding "$P" --member "serviceAccount:$SA" --role roles/biglake.editor --condition=None

ENV=MEMSTORE_PLATFORM=gcp,GCP_PROJECT=$P,GCS_BUCKET=$B
# Load job: commits a corpus dir (episodes.jsonl + facts.jsonl) from gs://$B/_corpora/<name>.
#   gcloud run jobs execute memstore-load --region $R --args=load,/data/_corpora/<name>,1000 --wait
gcloud run jobs create memstore-load --project "$P" --region "$R" --image "$I" --service-account "$SA" \
  --memory 8Gi --cpu 4 --task-timeout 3600 --max-retries 0 --set-env-vars "$ENV" \
  --add-volume name=corpora,type=cloud-storage,bucket="$B",readonly=true \
  --add-volume-mount volume=corpora,mount-path=/data --args=load,/data/_corpora/first,1000 || true
# Search service: private (IAM only), warm, index on GCS.
gcloud run deploy memstore-engine --project "$P" --region "$R" --image "$I" --service-account "$SA" \
  --no-allow-unauthenticated --args=serve,org,8080,"$G" \
  --set-env-vars "$ENV,MEMSTORE_SERVE_BIND=0.0.0.0,MEMSTORE_INDEX_URI=gs://$B/_index" \
  --memory 4Gi --cpu 2 --min-instances 1 --max-instances 1 --no-cpu-throttling --quiet

# BigQuery: a connection whose service account may invoke the engine and call Vertex (Gemini).
bq mk --connection --project_id "$P" --location "$R" --connection_type CLOUD_RESOURCE memstore || true
CSA=$(bq show --connection --format=json "$P.$R.memstore" | python3 -c "import json,sys;print(json.load(sys.stdin)['cloudResource']['serviceAccountId'])")
gcloud run services add-iam-policy-binding memstore-engine --project "$P" --region "$R" --member "serviceAccount:$CSA" --role roles/run.invoker
gcloud projects add-iam-policy-binding "$P" --member "serviceAccount:$CSA" --role roles/aiplatform.user --condition=None
bq mk --dataset --location "$R" "$P:memstore_api" || true
U=$(gcloud run services describe memstore-engine --project "$P" --region "$R" --format='value(status.url)')
bq query --project_id "$P" --use_legacy_sql=false "CREATE OR REPLACE FUNCTION \`$P.memstore_api.search\`(q STRING, k INT64)
  RETURNS JSON REMOTE WITH CONNECTION \`$P.$R.memstore\` OPTIONS (endpoint = '$U/search', max_batching_rows = 50)"
# An episode's text (NULL if missing or not visible), at most max_chars, centred on `around`.
bq query --project_id "$P" --use_legacy_sql=false "CREATE OR REPLACE FUNCTION \`$P.memstore_api.episode\`(episode_id STRING, around STRING, max_chars INT64)
  RETURNS STRING REMOTE WITH CONNECTION \`$P.$R.memstore\` OPTIONS (endpoint = '$U/episode', max_batching_rows = 50)"
# Each request runs as its caller: the Google identity in its ID token, or for BigQuery's calls the user whose
# query made them. BigQuery's tokens carry no email, so learn its subject id through /whoami first.
# The engine believes Cloud Run-verified tokens only after reading its own service and IAM policy (no allUsers).
gcloud run services add-iam-policy-binding memstore-engine --project "$P" --region "$R" --member "serviceAccount:$SA" --role roles/run.viewer >/dev/null
# Tokens with their signature intact (service accounts, BigQuery) must be minted for one of the service's URLs,
# which the engine reads from its own service.
gcloud run services update memstore-engine --project "$P" --region "$R" --quiet --update-env-vars MEMSTORE_CALLER_IDENTITY=google
bq query --project_id "$P" --use_legacy_sql=false "CREATE OR REPLACE FUNCTION \`$P.memstore_api._whoami\`()
  RETURNS STRING REMOTE WITH CONNECTION \`$P.$R.memstore\` OPTIONS (endpoint = '$U/whoami')"
BQ_ID=$(bq query --project_id "$P" --use_legacy_sql=false --format=csv "SELECT memstore_api._whoami()" | tail -1)
[[ $BQ_ID =~ ^[0-9]+$ ]] || { echo "unexpected BigQuery caller id: $BQ_ID"; exit 1; }
bq rm -f --routine "$P:memstore_api._whoami"
gcloud run services update memstore-engine --project "$P" --region "$R" --quiet --update-env-vars "MEMSTORE_BIGQUERY_CALLER=$BQ_ID"


# SQL surface: the inbox, remember() and ask() (deploy/gcp/sql; Terraform uses the same files).
MODEL=${MEMSTORE_ANSWER_MODEL:-gemini-2.5-flash}
for f in inbox remember ask; do
  sed -e "s/\${PROJECT}/$P/g" -e "s/\${DATASET}/memstore_api/g" -e "s/\${CONNECTION}/$R.memstore/g" -e "s/\${MODEL}/$MODEL/g" \
    "$(dirname "$0")/sql/$f.sql" |
    bq query --project_id "$P" --location "$R" --use_legacy_sql=false --quiet  # stdin: the files open with comments
done
# MCP remember_memory and ask_memory: the engine writes the inbox and calls Gemini as its own service account.
gcloud projects add-iam-policy-binding "$P" --member "serviceAccount:$SA" --role roles/bigquery.jobUser --condition=None >/dev/null
gcloud projects add-iam-policy-binding "$P" --member "serviceAccount:$SA" --role roles/aiplatform.user --condition=None >/dev/null
bq query --project_id "$P" --location "$R" --use_legacy_sql=false --quiet \
  "GRANT \`roles/bigquery.dataEditor\` ON TABLE \`$P.memstore_api.inbox\` TO 'serviceAccount:$SA'"
gcloud run services update memstore-engine --project "$P" --region "$R" --quiet \
  --update-env-vars "MEMSTORE_INBOX_TABLE=$P.memstore_api.inbox,MEMSTORE_ANSWER_MODEL=$MODEL,GCP_REGION=$R"

# Scheduled ingest: deploy/gcp/ingest.sh, unchanged, as a Cloud Run job on the Cloud SDK image (the script is read
# from the bucket), run by Cloud Scheduler as its own service account.
ISA=memstore-ingest@$P.iam.gserviceaccount.com
EVERY=${MEMSTORE_INGEST_MINUTES:-15}
gcloud services enable cloudscheduler.googleapis.com --project "$P"
gcloud iam service-accounts create memstore-ingest --project "$P" --display-name "memstore scheduled ingest" || true
gcloud storage cp "$(dirname "$0")/ingest.sh" "gs://$B/_bin/ingest.sh"
# A new service account takes a few seconds to be usable in IAM policies.
for _ in $(seq 12); do
  gcloud storage buckets add-iam-policy-binding "gs://$B" --member "serviceAccount:$ISA" --role roles/storage.objectAdmin >/dev/null 2>&1 && break
  sleep 5
done
# ponytail: project-level BigQuery roles; the Terraform module scopes data access to the memstore_api dataset.
for role in roles/bigquery.jobUser roles/bigquery.dataEditor roles/bigquery.connectionUser; do
  gcloud projects add-iam-policy-binding "$P" --member "serviceAccount:$ISA" --role $role --condition=None >/dev/null
done
# Runs memstore-load with per-batch args (runWithOverrides), and is itself run by the scheduler.
gcloud run jobs add-iam-policy-binding memstore-load --project "$P" --region "$R" --member "serviceAccount:$ISA" --role roles/run.developer >/dev/null
gcloud run jobs deploy memstore-ingest --project "$P" --region "$R" --image gcr.io/google.com/cloudsdktool/google-cloud-cli:slim \
  --service-account "$ISA" --memory 1Gi --task-timeout 3600 --max-retries 0 \
  --set-env-vars "GCP_PROJECT=$P,GCP_REGION=$R,GCS_BUCKET=$B${MEMSTORE_DEFAULT_ACL:+,MEMSTORE_DEFAULT_ACL=$MEMSTORE_DEFAULT_ACL}" \
  --add-volume name=bin,type=cloud-storage,bucket="$B",readonly=true --add-volume-mount volume=bin,mount-path=/mnt \
  --command bash --args=/mnt/_bin/ingest.sh,"${MEMSTORE_INGEST_MODEL:-gemini-2.5-flash}" --quiet
gcloud run jobs add-iam-policy-binding memstore-ingest --project "$P" --region "$R" --member "serviceAccount:$ISA" --role roles/run.invoker >/dev/null
gcloud scheduler jobs create http memstore-ingest --project "$P" --location "$R" --schedule "*/$EVERY * * * *" \
  --uri "https://run.googleapis.com/v2/projects/$P/locations/$R/jobs/memstore-ingest:run" --http-method POST \
  --oauth-service-account-email "$ISA" 2>/dev/null ||
gcloud scheduler jobs update http memstore-ingest --project "$P" --location "$R" --schedule "*/$EVERY * * * *"

cat <<EOF
Remember:   CALL memstore_api.remember('slack:#eng', 'We decided ...', NULL, ['group:$G']);
            (ingested every $EVERY min; now: gcloud run jobs execute memstore-ingest --region $R --wait)
Ask:        CALL memstore_api.ask('Who owns X?', TRUE);   -- FALSE = facts only
Search:     SELECT memstore_api.search('who owns X', 10);
Backfill:   gcloud run jobs execute memstore-load --region $R --args=backfill-quotes,org --wait
MCP:        $U/mcp, with an identity token of a principal holding roles/run.invoker on memstore-engine
Access:     each caller sees memories whose ACL names them (user:<email>), the default group, or *
EOF
