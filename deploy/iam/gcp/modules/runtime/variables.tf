variable "project" {
  description = "The customer's project id."
  type        = string
}
variable "project_number" {
  description = "The project's number (Secret Manager names secrets by it)."
  type        = string
}
variable "region" {
  type = string
}
variable "name" {
  description = "Prefix of every resource and of every name a grant is narrowed to."
  type        = string
}
variable "bucket" {
  description = "Name of the memory bucket this module creates (globally unique)."
  type        = string
}
variable "bucket_versioning" {
  description = "Keep overwritten and deleted objects as noncurrent versions."
  type        = bool
}
variable "secret_prefix" {
  description = "The engine may read secrets whose id starts with this, and no others."
  type        = string
}
variable "gke" {
  description = "The engine and the deployment agent run on GKE."
  type        = bool
}
variable "cloud_run" {
  description = "The engine runs on Cloud Run, with the BigQuery surface and scheduled ingest (deploy/gcp/terraform)."
  type        = bool
}
variable "k8s_namespace" {
  type = string
}
variable "engine_k8s_service_account" {
  type = string
}
variable "agent_k8s_service_account" {
  type = string
}
variable "gke_workload_identity_ready" {
  description = "A GKE cluster with Workload Identity exists in the project, so its identity pool can be named in bindings."
  type        = bool
}
variable "bigquery_connection_service_account" {
  description = "The service account of the BigQuery connection the vendor created; empty until it exists."
  type        = string
}
variable "cloud_run_deployed" {
  description = "The vendor has created the Cloud Run engine service and the load and ingest jobs (<name>-engine, -load, -ingest), so grants can be made on them."
  type        = bool
}
variable "scheduled_ingest" {
  description = "Cloud Run only: the scheduled-ingest job runs (<name>-ingest, as <name>-ingest@). deploy/gcp/terraform runs it; false for an install that deploys only the engine and the load job."
  type        = bool
}
