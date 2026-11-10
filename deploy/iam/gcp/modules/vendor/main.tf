# The vendor management identity: a service account in the customer's project that the vendor's own principal may
# impersonate, and only while var.vendor_access_enabled is true. Its roles let it build and run memstore's
# infrastructure (GKE or Cloud Run, the BigQuery surface, the network in BYOC) and nothing else:
#
#   - no IAM admin: it can't grant a role to anyone, itself included (no setIamPolicy anywhere);
#   - no data: it can't read or write the memory bucket's objects (beyond the ingest script under _bin/), read
#     BigQuery table data, or read a secret's value;
#   - no deletion of data stores: the bucket and dataset belong to the customer (../runtime), not to it.
#
# Every permission is listed with its scope and reason in docs/privileges.md. Together with ../runtime it is all
# deploy/gcp/terraform (existing_engine_sa) needs, as does the hosted service's onboarding wizard (not part of this
# repo): they enable no API, create no service account and make no grant, and check (read) what this module made.

locals {
  id = replace(var.name, "-", "_")

  base = concat(
    ["resourcemanager.projects.get", "compute.globalOperations.get", "compute.regionOperations.get",
    "compute.zoneOperations.get"],
    var.gke ? local.gke : [],
    var.cloud_run ? local.cloud_run_project : [],
    var.cloud_run && var.scheduled_ingest ? local.scheduler : [],
  )

  # GKE IAM permissions apply to the cluster's Kubernetes API too, in every namespace (GKE has no per-namespace
  # IAM). The list is what deploy/agent/k8s/agent.yaml and the engine's manifests create. It holds nothing for
  # cluster-wide objects (ClusterRoles, admission policies, CRDs), no Secret reads, no exec, attach, port-forward
  # or logs. The ValidatingAdmissionPolicy that limits the agent is applied by a cluster admin of the customer.
  gke = [
    "container.clusters.list",
    "container.operations.get", "container.operations.list",
    "container.namespaces.create", "container.namespaces.get",
    "container.serviceAccounts.create", "container.serviceAccounts.delete", "container.serviceAccounts.get",
    "container.serviceAccounts.update",
    "container.roles.bind", "container.roles.create", "container.roles.delete", "container.roles.escalate",
    "container.roles.get", "container.roles.update",
    "container.roleBindings.create", "container.roleBindings.delete", "container.roleBindings.get",
    "container.roleBindings.update",
    "container.configMaps.create", "container.configMaps.delete", "container.configMaps.get",
    "container.configMaps.update",
    "container.secrets.create", "container.secrets.delete", "container.secrets.update",
    "container.persistentVolumeClaims.create", "container.persistentVolumeClaims.delete",
    "container.persistentVolumeClaims.get", "container.persistentVolumeClaims.update",
    "container.deployments.create", "container.deployments.delete", "container.deployments.get",
    "container.deployments.getStatus", "container.deployments.update",
    "container.services.create", "container.services.delete", "container.services.get",
    "container.services.update",
    "container.jobs.create", "container.jobs.delete", "container.jobs.get", "container.jobs.getStatus",
    "container.jobs.update",
    "container.pods.get", "container.pods.list",
  ]

  # Project-level, for resources whose IAM can't be narrowed by name (or that don't exist yet).
  cloud_run_project = [
    "run.operations.get",
    "bigquery.connections.create", "bigquery.connections.delegate", "bigquery.connections.delete",
    "bigquery.connections.get", "bigquery.connections.update",
    "bigquery.jobs.create",
    "biglake.catalogs.create", "biglake.catalogs.get",
    # Reads, so the hosted onboarding wizard (not in this repo) can check this module's work instead of redoing it:
    # which APIs are on, and that the grants below and in ../runtime were made. Neither changes anything.
    "serviceusage.services.list", "resourcemanager.projects.getIamPolicy",
  ]

  # The scheduled-ingest trigger (deploy/gcp/terraform), only with var.scheduled_ingest.
  scheduler = [
    "cloudscheduler.jobs.create", "cloudscheduler.jobs.delete", "cloudscheduler.jobs.enable",
    "cloudscheduler.jobs.get", "cloudscheduler.jobs.pause", "cloudscheduler.jobs.update",
  ]

  # Conditions fail closed: a grant applies only to a resource of the named type whose name matches. A resource
  # the condition can't recognise (another type, or a service that gives IAM no resource attributes) gets nothing.
  # GKE clusters, Secret Manager and Compute Engine support resource attributes; Cloud Run does not, so its grant
  # (below) can't be narrowed and is project-wide.
  cluster_condition = join(" && ", [
    "resource.type == \"container.googleapis.com/Cluster\"",
    "resource.name.extract(\"/clusters/{c}\") == \"${var.name}\"",
  ])
  secret_condition = join(" && ", [
    "(resource.type == \"secretmanager.googleapis.com/Secret\" || resource.type == \"secretmanager.googleapis.com/SecretVersion\")",
    "resource.name.startsWith(\"projects/${var.project_number}/secrets/${var.secret_prefix}\")",
  ])
  network_condition = join(" || ", [
    "resource.type == \"compute.googleapis.com/Network\" && resource.name.extract(\"/networks/{n}\").startsWith(\"${var.name}\")",
    "resource.type == \"compute.googleapis.com/Subnetwork\" && resource.name.extract(\"/subnetworks/{n}\").startsWith(\"${var.name}\")",
    "resource.type == \"compute.googleapis.com/Router\" && resource.name.extract(\"/routers/{n}\").startsWith(\"${var.name}\")",
  ])

  # Service accounts the vendor attaches to workloads it creates (actAs), and nothing more on them.
  # (On GKE the engine and agent get their accounts through Workload Identity, which the customer binds.)
  act_as = merge(
    var.cloud_run ? { engine = var.engine_service_account } : {},
    var.cloud_run && var.scheduled_ingest ? { ingest = var.ingest_service_account } : {},
    var.gke ? { nodes = var.nodes_service_account } : {},
  )

  shared_vpc = !var.create_network && var.network_project != var.project
}

resource "google_service_account" "vendor" {
  project      = var.project
  account_id   = "${var.name}-vendor"
  display_name = "memstore vendor management (${var.name})"
  description  = "Impersonated by the memstore vendor to build and upgrade memstore here. Disabled while vendor access is off."
  # Disabled, it can mint no token, and Google should refuse tokens already minted (docs/privileges.md, Revoking).
  disabled = !var.vendor_access_enabled
}

# The one grant that lets the vendor in. Revoking access removes it (see the module README).
resource "google_service_account_iam_member" "vendor_impersonation" {
  count              = var.vendor_access_enabled ? 1 : 0
  service_account_id = google_service_account.vendor.name
  role               = "roles/iam.serviceAccountTokenCreator"
  member             = var.vendor_principal
}

# --- project-level role -----------------------------------------------------------------------------------------

resource "google_project_iam_custom_role" "base" {
  project     = var.project
  role_id     = "${local.id}_vendor_base"
  title       = "memstore vendor: infrastructure"
  description = "GKE cluster and workloads, Cloud Scheduler, the BigQuery connection and DDL jobs, the BigLake catalog, operation polling; reads of enabled APIs and the project IAM policy."
  permissions = local.base
}

resource "google_project_iam_member" "base" {
  project = var.project
  role    = google_project_iam_custom_role.base.id
  member  = google_service_account.vendor.member
}

# --- the GKE cluster named <name>, and no other -----------------------------------------------------------------

# Workload Identity trusts a namespace/service account in every cluster of the project, so a second cluster could
# impersonate the engine. The vendor can create, change and delete only the cluster named exactly var.name.
resource "google_project_iam_custom_role" "cluster" {
  count       = var.gke ? 1 : 0
  project     = var.project
  role_id     = "${local.id}_vendor_cluster"
  title       = "memstore vendor: its GKE cluster"
  description = "Create, upgrade, delete and get credentials for the cluster named for the install. Conditioned on that name."
  permissions = [
    "container.clusters.create", "container.clusters.delete", "container.clusters.get",
    "container.clusters.getCredentials", "container.clusters.update",
  ]
}

resource "google_project_iam_member" "cluster" {
  count   = var.gke ? 1 : 0
  project = var.project
  role    = google_project_iam_custom_role.cluster[0].id
  member  = google_service_account.vendor.member
  condition {
    title      = "cluster ${var.name} only"
    expression = local.cluster_condition
  }
}

# --- Cloud Run services and jobs ---------------------------------------------------------------------------------

# Cloud Run gives IAM conditions no resource attributes, and a create can only be granted on the project, so this
# is project-wide: the vendor can manage every Cloud Run service and job in the project. Use a project for memstore
# alone (docs/privileges.md). It holds no IAM and no execution, so it can't open a service to callers or run a job.

resource "google_project_iam_custom_role" "run" {
  count       = var.cloud_run ? 1 : 0
  project     = var.project
  role_id     = "${local.id}_vendor_run"
  title       = "memstore vendor: Cloud Run"
  description = "Create, update and delete the engine service and the load and ingest jobs; read a service's IAM policy. No IAM changes, no executions."
  permissions = [
    "run.jobs.create", "run.jobs.delete", "run.jobs.get", "run.jobs.update",
    "run.services.create", "run.services.delete", "run.services.get", "run.services.update",
    # Read only: the engine service must stay private, and its grants (../runtime) must be there.
    "run.services.getIamPolicy",
  ]
}

resource "google_project_iam_member" "run" {
  count   = var.cloud_run ? 1 : 0
  project = var.project
  role    = google_project_iam_custom_role.run[0].id
  member  = google_service_account.vendor.member
}

# --- Secret Manager: write, never read ---------------------------------------------------------------------------

resource "google_project_iam_custom_role" "secrets" {
  project     = var.project
  role_id     = "${local.id}_vendor_secrets"
  title       = "memstore vendor: write secrets"
  description = "Create secrets and add versions (the deployment credential). No secretmanager.versions.access."
  permissions = [
    "secretmanager.secrets.create", "secretmanager.secrets.get", "secretmanager.versions.add",
    "secretmanager.versions.get", "secretmanager.versions.list",
  ]
}

resource "google_project_iam_member" "secrets" {
  project = var.project
  role    = google_project_iam_custom_role.secrets.id
  member  = google_service_account.vendor.member
  condition {
    title      = "${var.secret_prefix}* secrets"
    expression = local.secret_condition
  }
}

# --- the memory bucket: metadata, and the ingest script under _bin/ ---------------------------------------------

resource "google_project_iam_custom_role" "bucket" {
  project     = var.project
  role_id     = "${local.id}_vendor_bucket"
  title       = "memstore vendor: see the memory bucket"
  description = "storage.buckets.get on the memory bucket (to point the catalog and jobs at it). No object access."
  permissions = ["storage.buckets.get"]
}

resource "google_storage_bucket_iam_member" "bucket" {
  bucket = var.bucket
  role   = google_project_iam_custom_role.bucket.id
  member = google_service_account.vendor.member
}

resource "google_project_iam_custom_role" "bin" {
  count       = var.cloud_run && var.scheduled_ingest ? 1 : 0
  project     = var.project
  role_id     = "${local.id}_vendor_bin"
  title       = "memstore vendor: the ingest script"
  description = "Upload and replace gs://<bucket>/_bin/*, the script the ingest job runs. Granted with a condition on that prefix."
  permissions = ["storage.objects.create", "storage.objects.delete", "storage.objects.get"]
}

resource "google_storage_bucket_iam_member" "bin" {
  count  = var.cloud_run && var.scheduled_ingest ? 1 : 0
  bucket = var.bucket
  role   = google_project_iam_custom_role.bin[0].id
  member = google_service_account.vendor.member
  condition {
    title      = "_bin/ only"
    expression = "resource.name.startsWith(\"projects/_/buckets/${var.bucket}/objects/_bin/\")"
  }
}

# --- the BigQuery dataset: DDL for routines and the inbox table, no table data ----------------------------------

resource "google_project_iam_custom_role" "ddl" {
  count       = var.cloud_run ? 1 : 0
  project     = var.project
  role_id     = "${local.id}_vendor_bq_ddl"
  title       = "memstore vendor: BigQuery routines and tables"
  description = "Create and replace the remote functions, remember/ask procedures and the inbox table. No getData, no updateData."
  permissions = [
    "bigquery.datasets.get",
    "bigquery.routines.create", "bigquery.routines.delete", "bigquery.routines.get", "bigquery.routines.update",
    "bigquery.tables.create", "bigquery.tables.get", "bigquery.tables.update",
  ]
}

resource "google_bigquery_dataset_iam_member" "ddl" {
  count      = var.cloud_run ? 1 : 0
  project    = var.project
  dataset_id = var.dataset
  role       = google_project_iam_custom_role.ddl[0].id
  member     = google_service_account.vendor.member
}

# --- attach the runtime service accounts to the workloads it creates ---------------------------------------------

resource "google_service_account_iam_member" "act_as" {
  for_each           = local.act_as
  service_account_id = "projects/${var.project}/serviceAccounts/${each.value}"
  role               = "roles/iam.serviceAccountUser"
  member             = google_service_account.vendor.member
}

# --- network: BYOC creates its own, BYOVPC uses the customer's ---------------------------------------------------

resource "google_project_iam_custom_role" "network" {
  count       = var.create_network ? 1 : 0
  project     = var.project
  role_id     = "${local.id}_vendor_network"
  title       = "memstore vendor: its own VPC"
  description = "Create and manage a VPC, subnets and a Cloud Router (Cloud NAT) named <name>*. No firewall rules, peering or routes."
  permissions = [
    "compute.networks.create", "compute.networks.delete", "compute.networks.get", "compute.networks.use",
    "compute.subnetworks.create", "compute.subnetworks.delete", "compute.subnetworks.get",
    "compute.subnetworks.setPrivateIpGoogleAccess", "compute.subnetworks.update", "compute.subnetworks.use",
    "compute.routers.create", "compute.routers.delete", "compute.routers.get", "compute.routers.update",
  ]
}

resource "google_project_iam_member" "network" {
  count   = var.create_network ? 1 : 0
  project = var.project
  role    = google_project_iam_custom_role.network[0].id
  member  = google_service_account.vendor.member
  condition {
    title      = "${var.name}* networks, subnets, routers"
    expression = local.network_condition
  }
}

# BYOVPC: use the customer's one subnet (create GKE nodes or Cloud Run egress in it), nothing else in the network.
resource "google_compute_subnetwork_iam_member" "subnet" {
  count      = var.create_network ? 0 : 1
  project    = var.network_project
  region     = var.region
  subnetwork = var.subnetwork
  role       = "roles/compute.networkUser"
  member     = google_service_account.vendor.member
}

# Shared VPC (the subnet is in another project): the Google service agents that build nodes and Cloud Run
# instances need the subnet too, and GKE's agent needs to manage firewall rules in the host project.
resource "google_compute_subnetwork_iam_member" "shared_vpc_agents" {
  for_each = local.shared_vpc ? merge(
    { cloudservices = "serviceAccount:${var.project_number}@cloudservices.gserviceaccount.com" },
    var.gke ? { gke = "serviceAccount:service-${var.project_number}@container-engine-robot.iam.gserviceaccount.com" } : {},
    var.cloud_run ? { run = "serviceAccount:service-${var.project_number}@serverless-robot-prod.iam.gserviceaccount.com" } : {},
  ) : {}
  project    = var.network_project
  region     = var.region
  subnetwork = var.subnetwork
  role       = "roles/compute.networkUser"
  member     = each.value
}

resource "google_project_iam_member" "shared_vpc_gke_host" {
  count   = local.shared_vpc && var.gke ? 1 : 0
  project = var.network_project
  role    = "roles/container.hostServiceAgentUser"
  member  = "serviceAccount:service-${var.project_number}@container-engine-robot.iam.gserviceaccount.com"
}
