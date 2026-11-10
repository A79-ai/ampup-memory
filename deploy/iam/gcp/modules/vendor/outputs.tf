output "service_account" {
  description = "The management service account the vendor impersonates."
  value       = google_service_account.vendor.email
}
output "enabled" {
  value = var.vendor_access_enabled
}
output "custom_roles" {
  description = "Every custom role granted to the management service account, with its permissions (for review and tests)."
  value = merge(
    { base = google_project_iam_custom_role.base.permissions },
    { secrets = google_project_iam_custom_role.secrets.permissions },
    { bucket = google_project_iam_custom_role.bucket.permissions },
    { for r in google_project_iam_custom_role.run : "run" => r.permissions },
    { for r in google_project_iam_custom_role.cluster : "cluster" => r.permissions },
    { for r in google_project_iam_custom_role.bin : "bin" => r.permissions },
    { for r in google_project_iam_custom_role.ddl : "ddl" => r.permissions },
    { for r in google_project_iam_custom_role.network : "network" => r.permissions },
  )
}
output "conditions" {
  description = "The IAM condition on each conditional grant."
  value = merge(
    { secrets = google_project_iam_member.secrets.condition[0].expression },
    { for b in google_project_iam_member.cluster : "cluster" => b.condition[0].expression },
    { for b in google_storage_bucket_iam_member.bin : "bin" => b.condition[0].expression },
    { for b in google_project_iam_member.network : "network" => b.condition[0].expression },
  )
}
output "impersonation_grants" {
  value = length(google_service_account_iam_member.vendor_impersonation)
}
output "disabled" {
  value = google_service_account.vendor.disabled
}
output "byovpc_subnet_grants" {
  value = length(google_compute_subnetwork_iam_member.subnet)
}
output "unconditioned_project_roles" {
  description = "Custom roles bound on the whole project with no condition (for review and tests)."
  value = concat(
    [google_project_iam_member.base.role],
    [for b in google_project_iam_member.run : b.role],
  )
}
