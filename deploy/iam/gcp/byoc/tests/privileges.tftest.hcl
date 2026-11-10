# Plans the module offline (no Google credentials; nothing is created) and checks the promises docs/privileges.md
# makes about the vendor's service account. Run: terraform init -backend=false && terraform test   (Terraform >= 1.7)

provider "google" {
  project      = "example-project"
  region       = "us-central1"
  access_token = "offline"
}

override_data {
  target = data.google_project.this
  values = { number = "123456789012" }
}

variables {
  project                             = "example-project"
  bucket                              = "example-memstore-data"
  vendor_principal                    = "serviceAccount:deployer@vendor-project.iam.gserviceaccount.com"
  cloud_run                           = true # every role the module can grant, GKE's and Cloud Run's
  dedicated_project                   = true
  cloud_run_deployed                  = true
  gke_workload_identity_ready         = true
  bigquery_connection_service_account = "bq-connection@example-project.iam.gserviceaccount.com"
}

run "vendor_is_least_privilege" {
  command = plan

  # No IAM administration, no data reads, no deletion of data stores, no Kubernetes exec or Secret reads.
  assert {
    condition = length([
      for p in flatten(values(module.vendor.custom_roles)) : p
      if endswith(p, ".setIamPolicy") || startswith(p, "iam.roles.") || startswith(p, "serviceusage.services.enable")
      || contains([
        "secretmanager.versions.access", "bigquery.tables.getData", "bigquery.tables.updateData",
        "bigquery.datasets.delete", "bigquery.datasets.update", "storage.buckets.delete", "storage.buckets.update",
        "storage.objects.list", "container.secrets.get", "container.secrets.list", "container.pods.exec",
        "container.pods.attach", "container.pods.portForward", "container.pods.getLogs", "container.clusterRoles.create",
        "container.clusterRoleBindings.create", "iam.serviceAccounts.getAccessToken", "iam.serviceAccountKeys.create",
        "run.jobs.run", "run.jobs.runWithOverrides",
      ], p)
    ]) == 0
    error_message = "the vendor can administer IAM, read data or delete a data store"
  }

  # Object writes only under _bin/ of the memory bucket.
  assert {
    condition = alltrue([
      for k, perms in module.vendor.custom_roles :
      k == "bin" || length([for p in perms : p if startswith(p, "storage.objects.")]) == 0
    ])
    error_message = "storage.objects.* may only be in the _bin/ role"
  }
  assert {
    condition     = module.vendor.conditions.bin == "resource.name.startsWith(\"projects/_/buckets/example-memstore-data/objects/_bin/\")"
    error_message = "the _bin/ role must be conditioned on that prefix"
  }

  # Named resources only.
  assert {
    condition     = strcontains(module.vendor.conditions.secrets, "startsWith(\"projects/123456789012/secrets/memstore-\")")
    error_message = "Secret Manager grants must be narrowed to the install's secrets"
  }
  assert {
    condition     = contains(keys(module.vendor.custom_roles), "network") && strcontains(module.vendor.conditions.network, "/networks/{n}")
    error_message = "BYOC: the vendor creates its own network, narrowed by name"
  }

  assert {
    condition     = module.vendor.impersonation_grants == 1 && module.vendor.disabled == false
    error_message = "vendor access is on by default"
  }
}

# Every condition fails closed: it grants only on a resource of a named type whose name matches, never on
# "anything that isn't type X" (the shape that let roles/run.invoker apply project-wide).
run "conditions_fail_closed" {
  command = plan

  assert {
    condition = alltrue([
      for c in concat(values(module.vendor.conditions), values(module.runtime.project_conditions)) :
      !strcontains(c, "!=") && !strcontains(c, "!(")
    ])
    error_message = "a condition has a negated (fail-open) clause"
  }
  assert {
    condition = alltrue([
      for k, c in module.vendor.conditions :
      k == "bin" || startswith(c, "resource.type ==") || startswith(c, "(resource.type ==")
    ])
    error_message = "a vendor condition must check the resource type before the name"
  }
}

# Cloud Run has no IAM resource attributes, so its grants are made on the engine service and the two jobs, never
# on the project; and only once they exist.
run "cloud_run_grants_per_resource" {
  command = plan

  assert {
    condition = (
      module.runtime.cloud_run_grants.connection_invokes_engine.resource == "service/memstore-engine" &&
      module.runtime.cloud_run_grants.connection_invokes_engine.role == "roles/run.invoker" &&
      module.runtime.cloud_run_grants.connection_invokes_engine.member == "serviceAccount:bq-connection@example-project.iam.gserviceaccount.com" &&
      module.runtime.cloud_run_grants.engine_run_self.resource == "service/memstore-engine" &&
      module.runtime.cloud_run_grants.ingest_runs_load.resource == "job/memstore-load" &&
      module.runtime.cloud_run_grants.scheduler_runs_ingest.resource == "job/memstore-ingest"
    )
    error_message = "Cloud Run grants must be on the engine service and the load and ingest jobs"
  }
  assert {
    condition = alltrue([
      for p in flatten(values(module.runtime.project_grants)) :
      !startswith(p, "roles/run.") && (!startswith(p, "run.") || p == "run.operations.get")
    ])
    error_message = "no runtime identity may hold a Cloud Run role or permission on the project (only operation status)"
  }
  assert {
    condition     = !contains(keys(module.vendor.conditions), "run")
    error_message = "no Cloud Run grant may pretend to be narrowed by a condition"
  }
}

# Workload Identity trusts memstore/memstore in every cluster of the project: the project must be memstore's alone,
# and the vendor may only create the cluster named for the install.
run "shared_project_refused" {
  command = plan

  variables {
    dedicated_project = false
  }

  expect_failures = [terraform_data.dedicated_project]
}

run "vendor_creates_only_its_cluster" {
  command = plan

  assert {
    condition     = module.vendor.conditions.cluster == "resource.type == \"container.googleapis.com/Cluster\" && resource.name.extract(\"/clusters/{c}\") == \"memstore\""
    error_message = "cluster lifecycle permissions must be limited to the cluster named for the install"
  }
  assert {
    condition = alltrue([
      for k, perms in module.vendor.custom_roles : k == "cluster" || length(setintersection(toset(perms), toset([
        "container.clusters.create", "container.clusters.update", "container.clusters.delete",
        "container.clusters.getCredentials",
      ]))) == 0
    ])
    error_message = "cluster create, update, delete and credentials may only be in the conditioned cluster role"
  }
  assert {
    condition = toset(module.runtime.workload_identity_members) == toset([
      "serviceAccount:example-project.svc.id.goog[memstore/memstore]",
      "serviceAccount:example-project.svc.id.goog[memstore/memstore-agent]",
    ])
    error_message = "Workload Identity binds exactly the engine's and the agent's Kubernetes service accounts"
  }
}
