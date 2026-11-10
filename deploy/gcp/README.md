# memstore on GCP

Two Terraform modules:

| Module | For | Engine | Extraction | Cost |
| --- | --- | --- | --- | --- |
| [`trial/`](trial) | a developer trying memstore | Cloud Run, scales to zero, 1 vCPU / 2 GiB | in-process, Claude Haiku 5.5 | ~$1–3 a month at light use, plus ~$5–10 once for 1,000 episodes |
| [`terraform/`](terraform) | production | Cloud Run, always on, 2 vCPU / 4 GiB, plus BigQuery remote functions and scheduled ingest | BigQuery `AI.GENERATE` (Gemini) | ~$111 a month before model use |

## A trial, step by step

You need a GCP project in which you may create resources and grant IAM roles (Owner, or Editor plus Project IAM
Admin: Editor alone is refused when the module binds the engine's service account), an engine image, and an
Anthropic API key. In Cloud Shell or
any terminal with `gcloud` and `terraform`:

```bash
P=<project>; R=us-central1
gcloud config set project $P

# 1. The Anthropic key, in Secret Manager (the engine reads it; it is never in Terraform state).
gcloud services enable secretmanager.googleapis.com
printf %s "$ANTHROPIC_API_KEY" | gcloud secrets create memstore-anthropic --data-file=-

# 2. The trial: a bucket and catalog, one engine that scales to zero, at most 1,000 episodes, ending in 30 days.
cd deploy/gcp/trial
terraform init
terraform apply -var project=$P -var bucket=$P-memstore-trial -var image=<engine image> \
  -var anthropic_key_secret=memstore-anthropic -var expires=$(date -v+30d +%F 2>/dev/null || date -d +30days +%F)
# A first apply can fail with a 403 from BigLake while the new IAM bindings propagate: apply again a minute later.
# The engine then refuses callers for up to 5 minutes, until it has confirmed Cloud Run IAM verifies them.

# 3. An org to write to (episodes sent to the deployment's default org come only from ingest), then connect an
#    agent: the engine is private; callers need roles/run.invoker on it, and send `x-memstore-org: <org>`.
MEMSTORE_PLATFORM=gcp GCP_PROJECT=$P GCS_BUCKET=$P-memstore-trial memstore org create <org> --admin <you> --direct
terraform output mcp
```

What the trial is and isn't:

- **Scales to zero.** The first request after an idle spell takes 5–15 s while the search index and embedding model
  load. An instance stays up about 15 minutes after the last request, with CPU on, and you are billed only while
  one is up. Extraction runs only while an instance is up: a backlog bigger than 15 minutes of extraction (a bulk
  load) waits for the next request. `-var min_instances=1` keeps one up until it drains (about $55 a month).
- **One instance, 1 vCPU.** It embeds each write and extracts in the background on the same CPU: a bulk load from
  8 parallel writers gets 429s ("no available instance") and the odd 5xx; retry them with backoff.
- **Capped at 1,000 episodes** (`MEMSTORE_MAX_EPISODES`). Past that, writes are refused with a 403, and reads and
  search go on.
- **Extraction uses Claude Haiku 5.5** (`MEMSTORE_EXTRACT=anthropic`). Measured on 50 of a trial's notes (about
  1.9 KB each): 1.8k tokens in and 1.7k out a note, most of the output Haiku's thinking, so about $10 for 1,000
  episodes at $1/$5 per million tokens. `-var extract_thinking=off` writes 0.7k a note (about $5 for 1,000) and
  found 12% fewer facts (175 against 199).
  A trial's schema names no episodes to extract, so it extracts every episode goal-blind
  (`MEMSTORE_EXTRACT_GOAL_BLIND=1`); memstore's default is to extract only what a scope schema's `extract` section
  names.
  `ask_memory` is off until the answer model can be Haiku too; `search_memory` works.
- **Labelled `memstore-trial=true`** and `memstore-expires=<date>`: `gcloud run services list --filter
  labels.memstore-trial=true` finds trials. Nothing deletes one on its own yet. To end it:
  `terraform destroy`, which also deletes the bucket and its data.
- **Leaves out the production module's BigQuery surface** (remote functions, inbox, scheduled ingest).

## Production: `terraform/`

```bash
cd deploy/gcp/terraform
terraform init
terraform apply -var project=$P -var bucket=<globally unique> -var image=<engine image>
```

It makes the bucket and its BigLake catalog, the engine on Cloud Run, the load and scheduled-ingest jobs, and the
BigQuery surface (remote functions, inbox, `remember`, `ask`). Every name carries `-var name=…` (default
`memstore`), so several installs can share a project.

To adopt a bucket and dataset you already own (as `deploy/iam`'s templates make them), pass
`-var existing_bucket=B -var existing_dataset=D` and leave `bucket` unset: the module reads and checks them and
never creates, changes or deletes them; its routines and inbox go in that dataset. See
[Who creates the bucket and the dataset](../../docs/privileges.md#who-creates-the-bucket-and-the-dataset).

To run on a template's vendor role alone, also pass `-var existing_engine_sa=… -var existing_ingest_sa=…` (the
template's `terraform_vars` output gives all four): the module then enables no API, creates no account and makes
no grant, and its `template_grants` check warns until the template's second apply. See
[Installing as the vendor role alone](../../docs/privileges.md#installing-as-the-vendor-role-alone).
