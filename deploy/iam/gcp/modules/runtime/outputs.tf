output "bucket" { value = google_storage_bucket.data.name }
output "dataset" { value = var.cloud_run ? google_bigquery_dataset.api[0].dataset_id : null }
output "engine_service_account" { value = google_service_account.engine.email }
output "ingest_service_account" { value = local.ingest ? google_service_account.ingest[0].email : null }
output "agent_service_account" { value = var.gke ? google_service_account.agent[0].email : null }
output "nodes_service_account" { value = var.gke ? google_service_account.nodes[0].email : null }
output "cloud_run_grants" {
  description = "Every Cloud Run grant, by where it is made (for review and tests). No Cloud Run grant is made on the project."
  value = merge(
    { for b in google_cloud_run_v2_service_iam_member.engine_run_self : "engine_run_self" => { resource = "service/${b.name}", role = b.role, member = b.member } },
    { for b in google_cloud_run_v2_service_iam_member.connection_invokes_engine : "connection_invokes_engine" => { resource = "service/${b.name}", role = b.role, member = b.member } },
    { for b in google_cloud_run_v2_job_iam_member.ingest_runs_load : "ingest_runs_load" => { resource = "job/${b.name}", role = b.role, member = b.member } },
    { for b in google_cloud_run_v2_job_iam_member.scheduler_runs_ingest : "scheduler_runs_ingest" => { resource = "job/${b.name}", role = b.role, member = b.member } },
  )
}
output "project_conditions" {
  description = "The IAM condition of every conditional project-level grant (for review and tests)."
  value = {
    engine_secrets = google_project_iam_member.engine_secrets.condition[0].expression
  }
}
output "workload_identity_members" {
  value = concat(
    [for b in google_service_account_iam_member.engine_workload_identity : b.member],
    [for b in google_service_account_iam_member.agent_workload_identity : b.member],
  )
}
output "project_grants" {
  description = "Every project-level grant to a runtime identity: the predefined role, or the custom role's permissions (for review and tests)."
  value = {
    engine_catalog        = google_project_iam_custom_role.catalog.permissions
    engine_predict        = google_project_iam_custom_role.predict.permissions
    engine_secrets        = ["roles/secretmanager.secretAccessor"]
    engine_job_user       = [for b in google_project_iam_member.engine_job_user : b.role]
    engine_connection     = flatten([for r in google_project_iam_custom_role.connection : r.permissions])
    ingest_job_user       = [for b in google_project_iam_member.ingest_job_user : b.role]
    ingest_connection     = flatten([for b in google_project_iam_member.ingest_connection : google_project_iam_custom_role.connection[0].permissions])
    ingest_run_operations = flatten([for b in google_project_iam_member.ingest_run_operations : google_project_iam_custom_role.run_operations[0].permissions])
    connection_predict    = flatten([for b in google_project_iam_member.connection_predict : google_project_iam_custom_role.predict.permissions])
    nodes                 = [for b in google_project_iam_member.nodes : b.role]
  }
}
