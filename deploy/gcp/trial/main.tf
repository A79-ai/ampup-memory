# A trial memstore on GCP, sized for a developer to try it: one engine on Cloud Run that scales to zero, a private
# bucket with a BigLake Iceberg catalog, facts extracted in-process by Claude Haiku 5.5, and at most 1,000 episodes.
# About $1–3 a month at light use, plus about $5–10 once to extract 1,000 episodes (see deploy/gcp/README.md).
# The production module is ../terraform; this one leaves out its BigQuery surface and scheduled ingest.
#
#   terraform apply -var project=<id> -var bucket=<globally unique> -var image=<engine image> \
#     -var anthropic_key_secret=<secret id holding an Anthropic API key> -var expires=2026-11-15

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
  description = "Prefix of every resource."
  type        = string
  default     = "memstore-trial"
}
variable "bucket" {
  description = "New bucket for the Iceberg tables, episode bodies and the search index."
  type        = string
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
variable "anthropic_key_secret" {
  description = "Secret Manager secret id (in this project) whose latest version is an Anthropic API key."
  type        = string
}
variable "extract_model" {
  type    = string
  default = "claude-haiku-5-5"
}
variable "extract_thinking" {
  description = "off: Haiku answers without thinking first, about half the extraction cost, for about 12% fewer facts."
  type        = string
  default     = "on"
}
variable "min_instances" {
  description = "1 keeps the engine up (about $55 a month), so extraction drains a backlog with no one calling it."
  type        = number
  default     = 0
}
variable "max_episodes" {
  type    = number
  default = 1000
}
variable "expires" {
  description = "When this trial ends (YYYY-MM-DD), as a label the control plane and `gcloud` can find it by."
  type        = string
  validation {
    condition     = can(regex("^\\d{4}-\\d{2}-\\d{2}$", var.expires))
    error_message = "expires is a date, YYYY-MM-DD."
  }
}
variable "deployment_credential_secret" {
  description = "Optional: a secret holding a deployment credential from the hosted service's onboarding (not part of this repo); the engine then reports its state to that control plane. Leave empty to self-host."
  type        = string
  default     = ""
}

locals {
  p      = var.project
  r      = var.region
  labels = { "memstore-install" = var.name, "memstore-trial" = "true", "memstore-expires" = var.expires }
}

# Never disabled on destroy: other workloads in the project may use them.
resource "google_project_service" "apis" {
  for_each           = toset(["run", "biglake", "iam", "storage", "secretmanager"])
  project            = local.p
  service            = "${each.key}.googleapis.com"
  disable_on_destroy = false
}

resource "google_storage_bucket" "data" {
  project                     = local.p
  name                        = var.bucket
  location                    = local.r
  uniform_bucket_level_access = true
  public_access_prevention    = "enforced"
  labels                      = local.labels
  force_destroy               = true # a trial: `terraform destroy` takes its data with it
  depends_on                  = [google_project_service.apis]
}

resource "google_biglake_iceberg_catalog" "catalog" {
  project         = local.p
  name            = google_storage_bucket.data.name
  catalog_type    = "CATALOG_TYPE_GCS_BUCKET"
  credential_mode = "CREDENTIAL_MODE_END_USER"
}

resource "google_service_account" "engine" {
  project      = local.p
  account_id   = "${var.name}-engine"
  display_name = "memstore trial engine (${var.name})"
  depends_on   = [google_project_service.apis]
}

resource "google_storage_bucket_iam_member" "data" {
  bucket = google_storage_bucket.data.name
  role   = "roles/storage.objectAdmin"
  member = "serviceAccount:${google_service_account.engine.email}"
}

resource "google_project_iam_member" "engine" {
  project = local.p
  role    = "roles/biglake.editor"
  member  = "serviceAccount:${google_service_account.engine.email}"
}

resource "google_secret_manager_secret_iam_member" "keys" {
  for_each  = toset(compact([var.anthropic_key_secret, var.deployment_credential_secret]))
  project   = local.p
  secret_id = each.key
  role      = "roles/secretmanager.secretAccessor"
  member    = "serviceAccount:${google_service_account.engine.email}"
}

resource "google_cloud_run_v2_service" "engine" {
  project             = local.p
  name                = "${var.name}-engine"
  location            = local.r
  labels              = local.labels
  deletion_protection = false
  template {
    service_account = google_service_account.engine.email
    labels          = local.labels
    scaling {
      # Scales to zero: billed only while an instance is up, about 15 minutes after the last request.
      min_instance_count = var.min_instances
      max_instance_count = 1
    }
    containers {
      image = var.image
      args  = ["serve", "org", "8080", var.audience]
      resources {
        limits = { cpu = "1", memory = "2Gi" }
        # CPU stays on while the instance is up, so extraction finishes after the request that wrote.
        cpu_idle          = false
        startup_cpu_boost = true
      }
      dynamic "env" {
        for_each = {
          MEMSTORE_PLATFORM           = "gcp"
          GCP_PROJECT                 = local.p
          GCP_REGION                  = local.r
          GCS_BUCKET                  = google_storage_bucket.data.name
          MEMSTORE_SERVE_BIND         = "0.0.0.0"
          MEMSTORE_INDEX_URI          = "gs://${google_storage_bucket.data.name}/_index"
          MEMSTORE_CALLER_IDENTITY    = "google"
          MEMSTORE_EXTRACT            = "anthropic"
          MEMSTORE_EXTRACT_MODEL      = var.extract_model
          MEMSTORE_EXTRACT_THINKING   = var.extract_thinking
          MEMSTORE_EXTRACT_GOAL_BLIND = "1" # a trial's schema extracts nothing of its own: every episode is extracted
          MEMSTORE_MAX_EPISODES       = tostring(var.max_episodes)
        }
        content {
          name  = env.key
          value = env.value
        }
      }
      env {
        name = "ANTHROPIC_API_KEY"
        value_source {
          secret_key_ref {
            secret  = var.anthropic_key_secret
            version = "latest"
          }
        }
      }
      dynamic "env" {
        for_each = var.deployment_credential_secret == "" ? [] : [1]
        content {
          name  = "MEMSTORE_DEPLOYMENT_CREDENTIAL"
          value = "/secrets/deployment/deployment.json"
        }
      }
      dynamic "volume_mounts" {
        for_each = var.deployment_credential_secret == "" ? [] : [1]
        content {
          name       = "deployment"
          mount_path = "/secrets/deployment"
        }
      }
    }
    dynamic "volumes" {
      for_each = var.deployment_credential_secret == "" ? [] : [1]
      content {
        name = "deployment"
        secret {
          secret = var.deployment_credential_secret
          items {
            version = "latest"
            path    = "deployment.json"
          }
        }
      }
    }
  }
  depends_on = [
    google_biglake_iceberg_catalog.catalog,
    google_storage_bucket_iam_member.data,
    google_project_iam_member.engine,
    google_secret_manager_secret_iam_member.keys,
  ]
}

# The engine believes Cloud Run-verified tokens only after reading its own service and IAM policy.
resource "google_cloud_run_v2_service_iam_member" "engine_self" {
  project  = local.p
  location = local.r
  name     = google_cloud_run_v2_service.engine.name
  role     = "roles/run.viewer"
  member   = "serviceAccount:${google_service_account.engine.email}"
}

output "engine_url" { value = google_cloud_run_v2_service.engine.uri }
output "mcp" {
  value = "${google_cloud_run_v2_service.engine.uri}/mcp (an ID token of a principal holding roles/run.invoker on ${google_cloud_run_v2_service.engine.name})"
}
output "expires" { value = var.expires }
