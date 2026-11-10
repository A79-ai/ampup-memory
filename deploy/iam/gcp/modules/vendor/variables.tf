variable "project" {
  type = string
}
variable "project_number" {
  type = string
}
variable "region" {
  type = string
}
variable "name" {
  type = string
}
variable "vendor_principal" {
  description = "The vendor's principal allowed to impersonate the management service account, as an IAM member."
  type        = string
}
variable "vendor_access_enabled" {
  description = "false removes the vendor's impersonation grant and disables the management service account."
  type        = bool
}
variable "gke" {
  type = bool
}
variable "cloud_run" {
  type = bool
}
variable "secret_prefix" {
  type = string
}
variable "bucket" {
  type = string
}
variable "dataset" {
  description = "The BigQuery dataset the runtime module created (null without Cloud Run)."
  type        = string
  default     = null
}
variable "engine_service_account" {
  type = string
}
variable "ingest_service_account" {
  type    = string
  default = null
}
variable "nodes_service_account" {
  type    = string
  default = null
}
variable "create_network" {
  description = "BYOC: the vendor creates its own VPC. BYOVPC: false, and network_project and subnetwork name the customer's."
  type        = bool
}
variable "network_project" {
  description = "BYOVPC: the project holding the customer's subnet (the Shared VPC host project, or this project)."
  type        = string
  default     = null
}
variable "subnetwork" {
  description = "BYOVPC: the name of the customer's subnet in var.region."
  type        = string
  default     = null
}
variable "scheduled_ingest" {
  description = "Cloud Run only: the vendor runs scheduled ingest (Cloud Scheduler, the ingest job, _bin/ingest.sh)."
  type        = bool
}
