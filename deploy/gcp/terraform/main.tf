# The memory on GCP as one Terraform module (successor to deploy/gcp/setup.sh): a private bucket with a BigLake
# Iceberg catalog, the engine on Cloud Run (each request runs as its caller), the load and scheduled-ingest jobs,
# and the BigQuery surface (remote functions, inbox, remember, ask). Several installs can share a project: every
# name carries var.name, and nothing here is authoritative over IAM or project services.
#   terraform apply -var project=<id> -var bucket=<globally unique> -var image=<engine image>
# With the customer's own bucket and dataset (made by deploy/iam/gcp/byoc or byovpc; its outputs say what to pass),
# they are read, checked and used, never created, changed or deleted:
#   terraform apply -var project=<id> -var existing_bucket=<bucket> -var existing_dataset=<dataset> -var image=...
# With the template's accounts too (existing_engine_sa, existing_ingest_sa; its `terraform_vars` output passes them
# all), it runs on the template's vendor role alone: it enables no API, creates no service account and makes no
# grant, all of which the template did; a check reports the grants still waiting for the template's second apply.

terraform {
  required_version = ">= 1.5"
  required_providers {
    google = { source = "hashicorp/google", version = ">= 6.30" }
  }
}

variable "project" { type = string }
variable "region" {
  type    = string
  default = "us-central1"
}
variable "name" {
  description = "Prefix of every resource; the BigQuery dataset made here is <name with _>_api."
  type        = string
  default     = "memstore"
}
variable "bucket" {
  description = "New bucket for the Iceberg tables, episode bodies and the search index. Leave empty with existing_bucket."
  type        = string
  default     = ""
}
variable "existing_bucket" {
  description = "The customer's own bucket (deploy/iam's `bucket` output): used as is, never created, changed or deleted here."
  type        = string
  default     = ""
}
variable "existing_dataset" {
  description = "The customer's own BigQuery dataset in var.region (deploy/iam's `dataset` output): its routines and inbox go in it; never created or deleted here."
  type        = string
  default     = ""
}
variable "existing_engine_sa" {
  description = "The template's engine account (<name>-engine@, deploy/iam's terraform_vars): run as, never created or granted on here. Set, the module makes no grant and enables no API (vendor role alone)."
  type        = string
  default     = ""
}
variable "existing_ingest_sa" {
  description = "The template's ingest account (<name>-ingest@), with existing_engine_sa."
  type        = string
  default     = ""
}
variable "image" {
  description = "The engine image (Artifact Registry)."
  type        = string
}
variable "audience" {
  description = "Group every caller is in: memories remembered without an ACL are shared with it."
  type        = string
  default     = "everyone"
}
variable "answer_model" {
  type    = string
  default = "gemini-2.5-flash"
}
variable "ingest_model" {
  type    = string
  default = "gemini-2.5-flash"
}
variable "ingest_minutes" {
  type    = number
  default = 15
}

locals {
  p        = var.project
  r        = var.region
  adopt_b  = var.existing_bucket != ""
  adopt_ds = var.existing_dataset != ""
  # The IAM template (deploy/iam) made the accounts, their grants and the APIs: none of them is made here.
  vendor_only    = var.existing_engine_sa != ""
  template_whole = !local.vendor_only || (var.existing_ingest_sa != "" && local.adopt_b && local.adopt_ds)
  owner          = local.vendor_only ? 0 : 1
  engine_sa      = one(local.vendor_only ? data.google_service_account.engine[*].email : google_service_account.engine[*].email)
  ingest_sa      = one(local.vendor_only ? (var.existing_ingest_sa == "" ? [""] : data.google_service_account.ingest[*].email) : google_service_account.ingest[*].email)
  # Each runtime account, for the grants made here (none with the template's).
  runtime       = local.vendor_only ? {} : { engine = local.engine_sa, ingest = local.ingest_sa }
  template_next = "-var cloud_run_deployed=true -var bigquery_connection_service_account=${google_bigquery_connection.conn.cloud_resource[0].service_account_id}"
  bucket        = local.adopt_b ? data.google_storage_bucket.existing[0].name : google_storage_bucket.data[0].name
  ds            = local.adopt_ds ? data.google_bigquery_dataset.existing[0].dataset_id : google_bigquery_dataset.api[0].dataset_id
  conn          = "${var.region}.${var.name}"
  sql_vars      = { PROJECT = local.p, DATASET = local.ds, CONNECTION = local.conn, MODEL = var.answer_model }
  labels        = { "memstore-install" = var.name }
  engine_url    = google_cloud_run_v2_service.engine.uri
}

# Never disabled on destroy: other workloads in the project may use them. With the template, it enabled them.
resource "google_project_service" "apis" {
  for_each = local.vendor_only ? toset([]) : toset(["run", "bigquery", "bigqueryconnection", "aiplatform", "biglake",
  "cloudscheduler", "cloudidentity", "iam", "storage"])
  project            = local.p
  service            = "${each.key}.googleapis.com"
  disable_on_destroy = false
}

# Exactly one of bucket and existing_bucket.
resource "terraform_data" "bucket_named" {
  lifecycle {
    precondition {
      condition     = (var.bucket == "") != (var.existing_bucket == "")
      error_message = "Name the bucket: -var bucket=<new bucket> to create one, or -var existing_bucket=<your bucket> to use yours (not both)."
    }
  }
}

# The template's accounts come with its bucket and dataset: adopting them alone would leave grants to make here.
resource "terraform_data" "template_whole" {
  lifecycle {
    precondition {
      condition     = local.template_whole
      error_message = "existing_engine_sa (the IAM template's accounts) needs existing_ingest_sa, existing_bucket and existing_dataset too: pass the template's whole `terraform_vars` output. No existing_ingest_sa? Apply the template with scheduled_ingest = true (this module runs scheduled ingest)."
    }
  }
}

# The template's accounts: read (iam.serviceAccounts.get, from roles/iam.serviceAccountUser on each).
data "google_service_account" "engine" {
  count      = local.vendor_only ? 1 : 0
  project    = local.p
  account_id = var.existing_engine_sa
}

data "google_service_account" "ingest" {
  count      = local.vendor_only && var.existing_ingest_sa != "" ? 1 : 0
  project    = local.p
  account_id = var.existing_ingest_sa
}

# The customer's bucket: read (storage.buckets.get, which deploy/iam grants the vendor), and checked.
data "google_storage_bucket" "existing" {
  count = local.adopt_b ? 1 : 0
  name  = var.existing_bucket
  lifecycle {
    postcondition {
      condition     = self.uniform_bucket_level_access && self.public_access_prevention == "enforced"
      error_message = "gs://${var.existing_bucket} must have uniform bucket-level access and public access prevented. Its owner runs: gcloud storage buckets update gs://${var.existing_bucket} --uniform-bucket-level-access --public-access-prevention"
    }
  }
}

resource "google_storage_bucket" "data" {
  count                       = local.adopt_b ? 0 : 1
  project                     = local.p
  name                        = var.bucket
  location                    = local.r
  uniform_bucket_level_access = true
  public_access_prevention    = "enforced"
  labels                      = local.labels
  force_destroy               = true # ponytail: test installs; a customer install should set false
  depends_on                  = [google_project_service.apis]
}

resource "google_biglake_iceberg_catalog" "catalog" {
  project      = local.p
  name         = local.bucket
  catalog_type = "CATALOG_TYPE_GCS_BUCKET"
  # The engine reads and writes the bucket as its own service account (no vended credentials).
  credential_mode = "CREDENTIAL_MODE_END_USER"
}

# --- identities -------------------------------------------------------------------------------------------------

resource "google_service_account" "engine" {
  count        = local.owner
  project      = local.p
  account_id   = "${var.name}-engine"
  display_name = "memstore engine (${var.name})"
  depends_on   = [google_project_service.apis]
}

resource "google_service_account" "ingest" {
  count        = local.owner
  project      = local.p
  account_id   = "${var.name}-ingest"
  display_name = "memstore scheduled ingest (${var.name})"
  depends_on   = [google_project_service.apis]
}

resource "google_storage_bucket_iam_member" "data" {
  for_each = local.runtime
  bucket   = local.bucket
  role     = "roles/storage.objectAdmin"
  member   = "serviceAccount:${each.value}"
}

resource "google_project_iam_member" "engine" {
  for_each = local.vendor_only ? toset([]) : toset(["roles/biglake.editor", "roles/bigquery.jobUser", "roles/aiplatform.user"])
  project  = local.p
  role     = each.key
  member   = "serviceAccount:${local.engine_sa}"
}

resource "google_project_iam_member" "ingest" {
  count   = local.owner
  project = local.p
  role    = "roles/bigquery.jobUser"
  member  = "serviceAccount:${local.ingest_sa}"
}

# --- BigQuery ---------------------------------------------------------------------------------------------------

resource "google_bigquery_connection" "conn" {
  project       = local.p
  location      = local.r
  connection_id = var.name
  cloud_resource {}
  depends_on = [google_project_service.apis]
}

resource "google_project_iam_member" "conn_vertex" {
  count   = local.owner
  project = local.p
  role    = "roles/aiplatform.user"
  member  = "serviceAccount:${google_bigquery_connection.conn.cloud_resource[0].service_account_id}"
}

# ingest calls Gemini through it; the engine calls its own _whoami through it.
resource "google_bigquery_connection_iam_member" "users" {
  for_each      = local.runtime
  project       = local.p
  location      = local.r
  connection_id = google_bigquery_connection.conn.connection_id
  role          = "roles/bigquery.connectionUser"
  member        = "serviceAccount:${each.value}"
}

# The customer's dataset: read (bigquery.datasets.get, which deploy/iam grants the vendor), and checked.
data "google_bigquery_dataset" "existing" {
  count      = local.adopt_ds ? 1 : 0
  project    = local.p
  dataset_id = var.existing_dataset
  lifecycle {
    postcondition {
      condition     = lower(self.location) == lower(local.r)
      error_message = "${local.p}:${var.existing_dataset} is in ${self.location}; its remote functions call the connection in ${local.r}. Pass a dataset in ${local.r}, or its owner makes one: bq mk --dataset --location ${local.r} ${local.p}:<dataset>"
    }
  }
}

resource "google_bigquery_dataset" "api" {
  count      = local.adopt_ds ? 0 : 1
  project    = local.p
  dataset_id = "${replace(var.name, "-", "_")}_api"
  location   = local.r
  labels     = local.labels
  # ponytail: test installs; a customer install should keep its inbox on destroy
  delete_contents_on_destroy = true
}

resource "google_bigquery_dataset_iam_member" "data" {
  for_each   = local.runtime
  project    = local.p
  dataset_id = local.ds
  role       = "roles/bigquery.dataEditor"
  member     = "serviceAccount:${each.value}"
}

resource "google_bigquery_routine" "search" {
  project         = local.p
  dataset_id      = local.ds
  routine_id      = "search"
  routine_type    = "SCALAR_FUNCTION"
  definition_body = ""
  return_type     = jsonencode({ typeKind = "JSON" })
  arguments {
    name      = "q"
    data_type = jsonencode({ typeKind = "STRING" })
  }
  arguments {
    name      = "k"
    data_type = jsonencode({ typeKind = "INT64" })
  }
  remote_function_options {
    endpoint          = "${local.engine_url}/search"
    connection        = google_bigquery_connection.conn.id
    max_batching_rows = "50"
  }
}

resource "google_bigquery_routine" "episode" {
  project         = local.p
  dataset_id      = local.ds
  routine_id      = "episode"
  routine_type    = "SCALAR_FUNCTION"
  definition_body = ""
  return_type     = jsonencode({ typeKind = "STRING" })
  arguments {
    name      = "episode_id"
    data_type = jsonencode({ typeKind = "STRING" })
  }
  arguments {
    name      = "around"
    data_type = jsonencode({ typeKind = "STRING" })
  }
  arguments {
    name      = "max_chars"
    data_type = jsonencode({ typeKind = "INT64" })
  }
  remote_function_options {
    endpoint          = "${local.engine_url}/episode"
    connection        = google_bigquery_connection.conn.id
    max_batching_rows = "50"
  }
}

# The engine calls this once at start, with a nonce only it knows, to learn BigQuery's caller id.
resource "google_bigquery_routine" "whoami" {
  project         = local.p
  dataset_id      = local.ds
  routine_id      = "_whoami"
  routine_type    = "SCALAR_FUNCTION"
  definition_body = ""
  return_type     = jsonencode({ typeKind = "STRING" })
  arguments {
    name      = "nonce"
    data_type = jsonencode({ typeKind = "STRING" })
  }
  remote_function_options {
    endpoint   = "${local.engine_url}/whoami"
    connection = google_bigquery_connection.conn.id
  }
}

# inbox, remember() and ask(): the same files setup.sh used, run as DDL jobs (a new job when a file changes).
resource "google_bigquery_job" "sql" {
  for_each = { for f in ["inbox", "remember", "ask"] : f => templatefile("${path.module}/../sql/${f}.sql", local.sql_vars) }
  project  = local.p
  location = local.r
  job_id   = "${local.ds}_${each.key}_${substr(sha256(each.value), 0, 12)}"
  labels   = local.labels
  query {
    query              = each.value
    use_legacy_sql     = false
    create_disposition = ""
    write_disposition  = ""
  }
  depends_on = [google_bigquery_routine.search, google_bigquery_routine.episode]
}

# --- engine -----------------------------------------------------------------------------------------------------

resource "google_cloud_run_v2_service" "engine" {
  project             = local.p
  name                = "${var.name}-engine"
  location            = local.r
  labels              = local.labels
  deletion_protection = false
  template {
    service_account = local.engine_sa
    scaling {
      min_instance_count = 1
      max_instance_count = 1
    }
    containers {
      image = var.image
      args  = ["serve", "org", "8080", var.audience]
      resources {
        limits            = { cpu = "2", memory = "4Gi" }
        cpu_idle          = false
        startup_cpu_boost = true
      }
      dynamic "env" {
        for_each = {
          MEMSTORE_PLATFORM        = "gcp"
          GCP_PROJECT              = local.p
          GCP_REGION               = local.r
          GCS_BUCKET               = local.bucket
          MEMSTORE_SERVE_BIND      = "0.0.0.0"
          MEMSTORE_INDEX_URI       = "gs://${local.bucket}/_index"
          MEMSTORE_CALLER_IDENTITY = "google"
          MEMSTORE_WHOAMI_FUNCTION = "${local.p}.${local.ds}._whoami"
          MEMSTORE_INBOX_TABLE     = "${local.p}.${local.ds}.inbox"
          MEMSTORE_ANSWER_MODEL    = var.answer_model
        }
        content {
          name  = env.key
          value = env.value
        }
      }
    }
  }
  depends_on = [google_biglake_iceberg_catalog.catalog, google_storage_bucket_iam_member.data, google_project_iam_member.engine]
}

# Callers: BigQuery's connection (for SQL); people and agents are granted run.invoker by the customer.
resource "google_cloud_run_v2_service_iam_member" "bigquery" {
  count    = local.owner
  project  = local.p
  location = local.r
  name     = google_cloud_run_v2_service.engine.name
  role     = "roles/run.invoker"
  member   = "serviceAccount:${google_bigquery_connection.conn.cloud_resource[0].service_account_id}"
}

# The engine believes Cloud Run-verified tokens only after reading its own service and IAM policy.
resource "google_cloud_run_v2_service_iam_member" "engine_self" {
  count    = local.owner
  project  = local.p
  location = local.r
  name     = google_cloud_run_v2_service.engine.name
  role     = "roles/run.viewer"
  member   = "serviceAccount:${local.engine_sa}"
}

# --- load and scheduled ingest ----------------------------------------------------------------------------------

resource "google_cloud_run_v2_job" "load" {
  project             = local.p
  name                = "${var.name}-load"
  location            = local.r
  labels              = local.labels
  deletion_protection = false
  template {
    template {
      service_account = local.engine_sa
      timeout         = "3600s"
      max_retries     = 0
      volumes {
        name = "corpora"
        gcs {
          bucket    = local.bucket
          read_only = true
        }
      }
      containers {
        image = var.image
        args  = ["load", "/data/_corpora/none", "1000"] # each run overrides the corpus dir
        resources {
          limits = { cpu = "4", memory = "8Gi" }
        }
        volume_mounts {
          name       = "corpora"
          mount_path = "/data"
        }
        dynamic "env" {
          for_each = { MEMSTORE_PLATFORM = "gcp", GCP_PROJECT = local.p, GCS_BUCKET = local.bucket }
          content {
            name  = env.key
            value = env.value
          }
        }
      }
    }
  }
}

resource "google_storage_bucket_object" "ingest_sh" {
  bucket = local.bucket
  name   = "_bin/ingest.sh"
  source = "${path.module}/../ingest.sh"
}

resource "google_cloud_run_v2_job" "ingest" {
  project             = local.p
  name                = "${var.name}-ingest"
  location            = local.r
  labels              = local.labels
  deletion_protection = false
  template {
    template {
      service_account = local.ingest_sa
      timeout         = "3600s"
      max_retries     = 0
      volumes {
        name = "bin"
        gcs {
          bucket    = local.bucket
          read_only = true
        }
      }
      containers {
        image   = "gcr.io/google.com/cloudsdktool/google-cloud-cli:slim"
        command = ["bash"]
        args    = ["/mnt/_bin/ingest.sh", var.ingest_model]
        resources {
          limits = { cpu = "1", memory = "1Gi" }
        }
        volume_mounts {
          name       = "bin"
          mount_path = "/mnt"
        }
        dynamic "env" {
          for_each = {
            GCP_PROJECT          = local.p
            GCP_REGION           = local.r
            GCS_BUCKET           = local.bucket
            MEMSTORE_DATASET     = local.ds
            MEMSTORE_CONNECTION  = local.conn
            MEMSTORE_LOAD_JOB    = google_cloud_run_v2_job.load.name
            MEMSTORE_DEFAULT_ACL = "group:${var.audience}"
          }
          content {
            name  = env.key
            value = env.value
          }
        }
      }
    }
  }
  depends_on = [google_storage_bucket_object.ingest_sh]
}

# Runs memstore-load with per-batch args (runWithOverrides), and is itself run by the scheduler.
resource "google_cloud_run_v2_job_iam_member" "ingest_runs_load" {
  count    = local.owner
  project  = local.p
  location = local.r
  name     = google_cloud_run_v2_job.load.name
  role     = "roles/run.developer"
  member   = "serviceAccount:${local.ingest_sa}"
}

resource "google_cloud_run_v2_job_iam_member" "scheduler_runs_ingest" {
  count    = local.owner
  project  = local.p
  location = local.r
  name     = google_cloud_run_v2_job.ingest.name
  role     = "roles/run.invoker"
  member   = "serviceAccount:${local.ingest_sa}"
}

resource "google_cloud_scheduler_job" "ingest" {
  project  = local.p
  region   = local.r
  name     = "${var.name}-ingest"
  schedule = "*/${var.ingest_minutes} * * * *"
  http_target {
    http_method = "POST"
    uri         = "https://run.googleapis.com/v2/projects/${local.p}/locations/${local.r}/jobs/${google_cloud_run_v2_job.ingest.name}:run"
    oauth_token {
      service_account_email = local.ingest_sa
    }
  }
}

# --- with the IAM template: its grants on what was deployed here --------------------------------------------------

# The template grants on the engine service and the jobs only once they exist (its cloud_run_deployed), and to the
# BigQuery connection's account only once it is named. Until its second apply the engine refuses callers; this
# says so (a warning, never a failure: the vendor can't make the grants). Reading the policy is the vendor role's
# run.services.getIamPolicy.
check "template_grants" {
  data "google_cloud_run_v2_service_iam_policy" "engine" {
    project  = local.p
    location = local.r
    name     = google_cloud_run_v2_service.engine.name
  }
  assert {
    # A conditional, not ||: without the template (or with half of it, refused above) the answer is known (true)
    # before anything exists.
    condition = local.vendor_only && local.template_whole ? length(setsubtract(
      [
        "projects/${local.p}/roles/${replace(var.name, "-", "_")}_engine_run_self serviceAccount:${local.engine_sa}",
        "roles/run.invoker serviceAccount:${google_bigquery_connection.conn.cloud_resource[0].service_account_id}",
      ],
      flatten([for b in try(jsondecode(data.google_cloud_run_v2_service_iam_policy.engine.policy_data).bindings, []) :
      [for m in b.members : "${b.role} ${m}"] if !can(b.condition)]),
    )) == 0 : true
    error_message = "The IAM template's grants on ${var.name}-engine aren't made yet, so the engine refuses callers. The customer applies deploy/iam/gcp/byoc (or byovpc) again with ${local.template_next} (and the -var flags it was applied with)."
  }
}

output "engine_url" { value = local.engine_url }
output "template_next" {
  description = "With the IAM template: what its owner adds to the template's apply once this module has deployed (null without it)."
  value       = local.vendor_only ? local.template_next : null
}
output "mcp" { value = "${local.engine_url}/mcp (ID token of a principal holding roles/run.invoker on ${google_cloud_run_v2_service.engine.name})" }
output "sql" {
  value = <<-EOT
    CALL ${local.ds}.remember('slack:#eng', 'We decided ...', NULL, ['group:${var.audience}']);
    CALL ${local.ds}.ask('Who owns X?', TRUE);
    SELECT ${local.ds}.search('who owns X', 10);
  EOT
}
