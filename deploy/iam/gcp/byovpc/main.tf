# BYOVPC on GCP: the customer brings the VPC and subnet (in this project or a Shared VPC host project), and
# this module creates every identity. The vendor builds memstore inside that subnet as a management service account
# with no network-creation permission. See README.md and docs/privileges.md.

data "google_project" "this" {
  project_id = var.project
}

# Workload Identity names the project's one identity pool: a pod running as memstore/memstore in ANY GKE cluster of
# this project would be the engine. GKE has no binding that names one cluster and one service account, and Cloud
# Run grants can't be narrowed by name, so the project must be memstore's alone (docs/privileges.md).
resource "terraform_data" "dedicated_project" {
  count = var.gke || var.cloud_run ? 1 : 0
  lifecycle {
    precondition {
      condition     = var.dedicated_project
      error_message = "memstore needs a project of its own. On GKE, Workload Identity would trust the engine's Kubernetes service account in every cluster of ${var.project}; on Cloud Run, the vendor's role covers every Cloud Run service in it. Use a dedicated project (or make sure no one else can create GKE clusters or Cloud Run services in it) and set dedicated_project = true."
    }
  }
}

# The customer enables the APIs, so the vendor needs no serviceusage permission. Never disabled on destroy.
resource "google_project_service" "apis" {
  for_each = toset(concat(
    ["iam", "iamcredentials", "storage", "biglake", "aiplatform", "secretmanager", "compute"],
    var.gke ? ["container"] : [],
    # cloudidentity: the engine's group lookups. cloudresourcemanager: the project IAM policy, read to check grants.
    var.cloud_run ? ["run", "bigquery", "bigqueryconnection", "cloudidentity", "cloudresourcemanager"] : [],
    var.cloud_run && var.scheduled_ingest ? ["cloudscheduler"] : [],
  ))
  project            = var.project
  service            = "${each.key}.googleapis.com"
  disable_on_destroy = false
}

module "runtime" {
  source = "../modules/runtime"

  project                             = var.project
  project_number                      = data.google_project.this.number
  region                              = var.region
  name                                = var.name
  bucket                              = var.bucket
  bucket_versioning                   = var.bucket_versioning
  secret_prefix                       = var.secret_prefix
  gke                                 = var.gke
  cloud_run                           = var.cloud_run
  k8s_namespace                       = var.k8s_namespace
  engine_k8s_service_account          = var.engine_k8s_service_account
  agent_k8s_service_account           = var.agent_k8s_service_account
  gke_workload_identity_ready         = var.gke_workload_identity_ready
  cloud_run_deployed                  = var.cloud_run_deployed
  scheduled_ingest                    = var.scheduled_ingest
  bigquery_connection_service_account = var.bigquery_connection_service_account

  depends_on = [google_project_service.apis, terraform_data.dedicated_project]
}

module "vendor" {
  source = "../modules/vendor"

  project                = var.project
  project_number         = data.google_project.this.number
  region                 = var.region
  name                   = var.name
  vendor_principal       = var.vendor_principal
  vendor_access_enabled  = var.vendor_access_enabled
  gke                    = var.gke
  cloud_run              = var.cloud_run
  scheduled_ingest       = var.scheduled_ingest
  secret_prefix          = var.secret_prefix
  bucket                 = module.runtime.bucket
  dataset                = module.runtime.dataset
  engine_service_account = module.runtime.engine_service_account
  ingest_service_account = module.runtime.ingest_service_account
  nodes_service_account  = module.runtime.nodes_service_account

  create_network  = false
  network_project = var.network_project
  subnetwork      = var.subnetwork

  depends_on = [google_project_service.apis]
}
