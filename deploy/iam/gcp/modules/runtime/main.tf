# The identities memstore runs as in the customer's project, and the data stores they own. The customer applies
# this (through ../../byoc or ../../byovpc), never the vendor: the vendor's role holds no IAM-admin permission, so
# every grant a workload needs is made here. Each permission is listed with its reason in docs/privileges.md.
#
# The bucket and the BigQuery dataset are created here, not by the vendor, so the customer owns them outright:
# revoking the vendor's access, or destroying the vendor's infrastructure, leaves the data where it is.

locals {
  id     = replace(var.name, "-", "_")
  labels = { "memstore-install" = var.name }
  # Workload Identity principal of a Kubernetes service account in this project's GKE clusters. It names the
  # project's one identity pool, so the same namespace and service account in ANY cluster of the project match:
  # that is why the GKE variants require a dedicated project (var.dedicated_project in ../../byoc, ../../byovpc).
  wi = "serviceAccount:${var.project}.svc.id.goog"
  # Cloud Run supports no resource attributes in IAM conditions, so every Cloud Run grant is made on the service or
  # job itself, once the vendor has created it (var.cloud_run_deployed). These are the names deploy/gcp/terraform
  # gives them.
  engine_service = "${var.name}-engine"
  load_job       = "${var.name}-load"
  ingest_job     = "${var.name}-ingest"
  run_bound      = var.cloud_run && var.cloud_run_deployed
  # The scheduled-ingest account and its grants: deploy/gcp/terraform runs it; off with scheduled_ingest = false.
  ingest = var.cloud_run && var.scheduled_ingest
}

# --- data stores (customer-owned) -------------------------------------------------------------------------------

resource "google_storage_bucket" "data" {
  project                     = var.project
  name                        = var.bucket
  location                    = var.region
  uniform_bucket_level_access = true
  public_access_prevention    = "enforced"
  labels                      = local.labels
  force_destroy               = false # never takes the memory with it
  versioning {
    enabled = var.bucket_versioning
  }
}

resource "google_bigquery_dataset" "api" {
  count                      = var.cloud_run ? 1 : 0
  project                    = var.project
  dataset_id                 = "${local.id}_api"
  location                   = var.region
  labels                     = local.labels
  delete_contents_on_destroy = false
}

# --- service accounts -------------------------------------------------------------------------------------------

resource "google_service_account" "engine" {
  project      = var.project
  account_id   = "${var.name}-engine"
  display_name = "memstore engine (${var.name})"
  description  = "Runs the memstore engine. Reads and writes the memory bucket; nothing else in the project."
}

resource "google_service_account" "ingest" {
  count        = local.ingest ? 1 : 0
  project      = var.project
  account_id   = "${var.name}-ingest"
  display_name = "memstore scheduled ingest (${var.name})"
  description  = "Runs the scheduled ingest job (Cloud Run job, started by Cloud Scheduler)."
}

resource "google_service_account" "agent" {
  count        = var.gke ? 1 : 0
  project      = var.project
  account_id   = "${var.name}-agent"
  display_name = "memstore deployment agent (${var.name})"
  description  = "The in-cluster deployment agent. Holds no role in this project: it exists so the vendor's registry can let it read image signatures."
}

resource "google_service_account" "nodes" {
  count        = var.gke ? 1 : 0
  project      = var.project
  account_id   = "${var.name}-nodes"
  display_name = "memstore GKE nodes (${var.name})"
  description  = "GKE node identity: writes logs and metrics, pulls images. Replaces the Compute Engine default account."
}

# --- custom roles -----------------------------------------------------------------------------------------------

resource "google_project_iam_custom_role" "objects" {
  project     = var.project
  role_id     = "${local.id}_engine_objects"
  title       = "memstore engine: objects in its bucket"
  description = "Read, write, list and delete objects. Granted on the memory bucket only."
  permissions = ["storage.objects.create", "storage.objects.delete", "storage.objects.get", "storage.objects.list"]
}

# roles/biglake.editor without catalog create/update/delete and without IAM reads: the engine works inside the
# catalog the vendor created, it never makes or removes one.
resource "google_project_iam_custom_role" "catalog" {
  project     = var.project
  role_id     = "${local.id}_engine_catalog"
  title       = "memstore engine: BigLake Iceberg catalog"
  description = "Namespaces and tables in the BigLake Iceberg REST catalog over the memory bucket."
  permissions = [
    "biglake.catalogs.get",
    "biglake.namespaces.create", "biglake.namespaces.delete", "biglake.namespaces.get", "biglake.namespaces.list",
    "biglake.namespaces.update",
    "biglake.tables.create", "biglake.tables.delete", "biglake.tables.get", "biglake.tables.list",
    "biglake.tables.lock", "biglake.tables.update",
  ]
}

resource "google_project_iam_custom_role" "predict" {
  project     = var.project
  role_id     = "${local.id}_model_predict"
  title       = "memstore: call Gemini on Vertex AI"
  description = "aiplatform.endpoints.predict only (generateContent and the OpenAI-compatible endpoint)."
  permissions = ["aiplatform.endpoints.predict"]
}

resource "google_project_iam_custom_role" "run_self" {
  count       = var.cloud_run ? 1 : 0
  project     = var.project
  role_id     = "${local.id}_engine_run_self"
  title       = "memstore engine: read its own Cloud Run service"
  description = "The engine trusts Cloud Run-verified tokens only after reading its service and its IAM policy."
  permissions = ["run.services.get", "run.services.getIamPolicy"]
}

resource "google_project_iam_custom_role" "connection" {
  count       = var.cloud_run ? 1 : 0
  project     = var.project
  role_id     = "${local.id}_bq_connection_use"
  title       = "memstore: use the BigQuery connection"
  description = "Call remote functions and AI.GENERATE through the BigQuery CLOUD_RESOURCE connection."
  permissions = ["bigquery.connections.get", "bigquery.connections.use"]
}

resource "google_project_iam_custom_role" "run_jobs" {
  count       = local.ingest ? 1 : 0
  project     = var.project
  role_id     = "${local.id}_ingest_run_jobs"
  title       = "memstore ingest: run its Cloud Run jobs"
  description = "Run the load job with per-batch arguments and wait for it. Granted on that job only."
  permissions = ["run.executions.get", "run.jobs.get", "run.jobs.run", "run.jobs.runWithOverrides"]
}

# Waiting on `gcloud run jobs execute --wait`: operation status. Cloud Run operations can't be bound per resource.
resource "google_project_iam_custom_role" "run_operations" {
  count       = local.ingest ? 1 : 0
  project     = var.project
  role_id     = "${local.id}_run_operations"
  title       = "memstore ingest: read Cloud Run operation status"
  description = "run.operations.get only: the status of a long-running Cloud Run call it made."
  permissions = ["run.operations.get"]
}

# --- engine -----------------------------------------------------------------------------------------------------

resource "google_storage_bucket_iam_member" "engine" {
  bucket = google_storage_bucket.data.name
  role   = google_project_iam_custom_role.objects.id
  member = google_service_account.engine.member
}

resource "google_project_iam_member" "engine_catalog" {
  project = var.project
  role    = google_project_iam_custom_role.catalog.id
  member  = google_service_account.engine.member
}

resource "google_project_iam_member" "engine_predict" {
  project = var.project
  role    = google_project_iam_custom_role.predict.id
  member  = google_service_account.engine.member
}

# Secrets the engine reads (a model API key, the deployment credential): only those named <secret_prefix>*.
resource "google_project_iam_member" "engine_secrets" {
  project = var.project
  role    = "roles/secretmanager.secretAccessor"
  member  = google_service_account.engine.member
  condition {
    title      = "${var.secret_prefix}* secrets only"
    expression = "resource.name.startsWith(\"projects/${var.project_number}/secrets/${var.secret_prefix}\")"
  }
}

resource "google_cloud_run_v2_service_iam_member" "engine_run_self" {
  count    = local.run_bound ? 1 : 0
  project  = var.project
  location = var.region
  name     = local.engine_service
  role     = google_project_iam_custom_role.run_self[0].id
  member   = google_service_account.engine.member
}

resource "google_project_iam_member" "engine_job_user" {
  count   = var.cloud_run ? 1 : 0
  project = var.project
  role    = "roles/bigquery.jobUser"
  member  = google_service_account.engine.member
}

resource "google_bigquery_dataset_iam_member" "engine" {
  count      = var.cloud_run ? 1 : 0
  project    = var.project
  dataset_id = google_bigquery_dataset.api[0].dataset_id
  role       = "roles/bigquery.dataEditor"
  member     = google_service_account.engine.member
}

resource "google_project_iam_member" "engine_connection" {
  count   = var.cloud_run ? 1 : 0
  project = var.project
  role    = google_project_iam_custom_role.connection[0].id
  member  = google_service_account.engine.member
}

resource "google_service_account_iam_member" "engine_workload_identity" {
  count              = var.gke && var.gke_workload_identity_ready ? 1 : 0
  service_account_id = google_service_account.engine.name
  role               = "roles/iam.workloadIdentityUser"
  member             = "${local.wi}[${var.k8s_namespace}/${var.engine_k8s_service_account}]"
}

# --- scheduled ingest (Cloud Run) -------------------------------------------------------------------------------

resource "google_storage_bucket_iam_member" "ingest" {
  count  = local.ingest ? 1 : 0
  bucket = google_storage_bucket.data.name
  role   = google_project_iam_custom_role.objects.id
  member = google_service_account.ingest[0].member
}

resource "google_project_iam_member" "ingest_job_user" {
  count   = local.ingest ? 1 : 0
  project = var.project
  role    = "roles/bigquery.jobUser"
  member  = google_service_account.ingest[0].member
}

resource "google_bigquery_dataset_iam_member" "ingest" {
  count      = local.ingest ? 1 : 0
  project    = var.project
  dataset_id = google_bigquery_dataset.api[0].dataset_id
  role       = "roles/bigquery.dataEditor"
  member     = google_service_account.ingest[0].member
}

resource "google_project_iam_member" "ingest_connection" {
  count   = local.ingest ? 1 : 0
  project = var.project
  role    = google_project_iam_custom_role.connection[0].id
  member  = google_service_account.ingest[0].member
}

resource "google_cloud_run_v2_job_iam_member" "ingest_runs_load" {
  count    = local.run_bound && local.ingest ? 1 : 0
  project  = var.project
  location = var.region
  name     = local.load_job
  role     = google_project_iam_custom_role.run_jobs[0].id
  member   = google_service_account.ingest[0].member
}

# Cloud Scheduler starts the ingest job with the ingest account's token.
resource "google_cloud_run_v2_job_iam_member" "scheduler_runs_ingest" {
  count    = local.run_bound && local.ingest ? 1 : 0
  project  = var.project
  location = var.region
  name     = local.ingest_job
  role     = "roles/run.invoker"
  member   = google_service_account.ingest[0].member
}

resource "google_project_iam_member" "ingest_run_operations" {
  count   = local.ingest ? 1 : 0
  project = var.project
  role    = google_project_iam_custom_role.run_operations[0].id
  member  = google_service_account.ingest[0].member
}

# --- BigQuery's connection (its service account exists only once the vendor has made the connection) ----------

resource "google_project_iam_member" "connection_predict" {
  count   = var.cloud_run && var.bigquery_connection_service_account != "" ? 1 : 0
  project = var.project
  role    = google_project_iam_custom_role.predict.id
  member  = "serviceAccount:${var.bigquery_connection_service_account}"
}

# On the engine service only: BigQuery's remote functions call it, and nothing else in the project.
resource "google_cloud_run_v2_service_iam_member" "connection_invokes_engine" {
  count    = local.run_bound && var.bigquery_connection_service_account != "" ? 1 : 0
  project  = var.project
  location = var.region
  name     = local.engine_service
  role     = "roles/run.invoker"
  member   = "serviceAccount:${var.bigquery_connection_service_account}"
}

# --- agent and nodes (GKE) --------------------------------------------------------------------------------------

resource "google_service_account_iam_member" "agent_workload_identity" {
  count              = var.gke && var.gke_workload_identity_ready ? 1 : 0
  service_account_id = google_service_account.agent[0].name
  role               = "roles/iam.workloadIdentityUser"
  member             = "${local.wi}[${var.k8s_namespace}/${var.agent_k8s_service_account}]"
}

resource "google_project_iam_member" "nodes" {
  count   = var.gke ? 1 : 0
  project = var.project
  role    = "roles/container.defaultNodeServiceAccount"
  member  = google_service_account.nodes[0].member
}
