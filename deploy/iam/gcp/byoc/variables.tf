variable "project" {
  description = "The customer's project memstore is installed in."
  type        = string
}
variable "region" {
  type    = string
  default = "us-central1"
}
variable "name" {
  description = "Prefix of every resource and of every name a grant is narrowed to (service account ids add up to 9 characters)."
  type        = string
  default     = "memstore"
  validation {
    condition     = can(regex("^[a-z][a-z0-9-]{2,20}$", var.name))
    error_message = "name is 3 to 21 lowercase letters, digits and dashes, starting with a letter."
  }
}
variable "bucket" {
  description = "The memory bucket this module creates and the customer owns (globally unique)."
  type        = string
}
variable "bucket_versioning" {
  description = "Keep overwritten and deleted objects as noncurrent versions (recoverable, billed)."
  type        = bool
  default     = true
}
variable "vendor_principal" {
  description = "The vendor's principal that may impersonate the management service account, e.g. serviceAccount:deployer@<vendor project>.iam.gserviceaccount.com."
  type        = string
  validation {
    condition     = can(regex("^(serviceAccount|principal|principalSet):", var.vendor_principal))
    error_message = "vendor_principal is an IAM member: serviceAccount:..., principal://... or principalSet://..."
  }
}
variable "vendor_access_enabled" {
  description = "Set false to revoke the vendor's access. Runtime identities and data are kept."
  type        = bool
  default     = true
}
variable "gke" {
  description = "The engine and the deployment agent run on GKE."
  type        = bool
  default     = true
}
variable "cloud_run" {
  description = "The engine runs on Cloud Run with the BigQuery surface and scheduled ingest (what deploy/gcp/terraform builds)."
  type        = bool
  default     = false
}
variable "secret_prefix" {
  description = "Secrets the engine may read and the vendor may write start with this."
  type        = string
  default     = "memstore-"
}
variable "k8s_namespace" {
  type    = string
  default = "memstore"
}
variable "engine_k8s_service_account" {
  type    = string
  default = "memstore"
}
variable "agent_k8s_service_account" {
  type    = string
  default = "memstore-agent"
}
variable "gke_workload_identity_ready" {
  description = "Set true and apply again once the vendor's GKE cluster exists: Workload Identity bindings name its identity pool, which exists only then."
  type        = bool
  default     = false
}
variable "dedicated_project" {
  description = "Required: confirm the project is memstore's alone, and no one else may create GKE clusters or Cloud Run services in it. Workload Identity trusts the engine's namespace and service account in every cluster of the project, and the vendor's Cloud Run grant can't be narrowed by name."
  type        = bool
  default     = false
}
variable "scheduled_ingest" {
  description = "Cloud Run only: scheduled ingest runs (deploy/gcp/terraform). Set false for an install that deploys the engine and the load job only: no ingest account, no <name>-ingest grants, no Cloud Scheduler for the vendor."
  type        = bool
  default     = true
}
variable "cloud_run_deployed" {
  description = "Cloud Run only: set true and apply again once the vendor has created <name>-engine, <name>-load and (with scheduled_ingest) <name>-ingest; their IAM grants are made on them."
  type        = bool
  default     = false
}
variable "bigquery_connection_service_account" {
  description = "Cloud Run only: the BigQuery connection's service account, once the vendor has created it. Lets BigQuery call Gemini and the engine."
  type        = string
  default     = ""
}
