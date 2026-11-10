output "vendor_service_account" {
  description = "Give this to the vendor: the account it impersonates to install and upgrade memstore."
  value       = module.vendor.service_account
}
output "vendor_access_enabled" {
  value = module.vendor.enabled
}
output "bucket" {
  description = "The memory bucket, the customer's: the vendor adopts it, never creates it (deploy/gcp/terraform existing_bucket)."
  value       = module.runtime.bucket
}
output "dataset" {
  description = "The BigQuery dataset (cloud_run = true only), the customer's: adopted the same way (deploy/gcp/terraform existing_dataset)."
  value       = module.runtime.dataset
}
output "init_flags" {
  description = "Flags for the hosted service's onboarding wizard (not part of this repo): they adopt this module's bucket and dataset, and with cloud_run = true run as the vendor role alone. Self-hosted installs use terraform_vars."
  value       = "--existing-bucket ${module.runtime.bucket}${module.runtime.dataset == null ? "" : " --existing-dataset ${module.runtime.dataset} --iam-template ${var.name}"}"
}
output "terraform_vars" {
  description = "For deploy/gcp/terraform: -var flags that adopt this module's bucket and dataset, and with cloud_run = true its engine and ingest accounts (vendor role alone)."
  value = join(" ", concat(
    module.runtime.dataset == null ? [] : ["-var project=${var.project}", "-var region=${var.region}", "-var name=${var.name}"],
    ["-var existing_bucket=${module.runtime.bucket}"],
    module.runtime.dataset == null ? [] : ["-var existing_dataset=${module.runtime.dataset}", "-var existing_engine_sa=${var.name}-engine@${var.project}.iam.gserviceaccount.com"],
    # Without scheduled ingest there is no ingest account, and deploy/gcp/terraform (which runs ingest) refuses.
    var.cloud_run && var.scheduled_ingest ? ["-var existing_ingest_sa=${var.name}-ingest@${var.project}.iam.gserviceaccount.com"] : [],
  ))
}
output "after_vendor_deploys" {
  description = "Cloud Run: once the vendor has deployed, apply this module again with these too, so the grants made on the engine service, the jobs and the BigQuery connection's account exist (the vendor can't make them)."
  value       = var.cloud_run ? "-var cloud_run_deployed=true -var bigquery_connection_service_account=<cloudResource.serviceAccountId of: bq show --format=json --connection ${var.project}.${var.region}.${var.name}>" : null
}
output "engine_service_account" {
  value = module.runtime.engine_service_account
}
output "ingest_service_account" {
  value = module.runtime.ingest_service_account
}
output "agent_service_account" {
  description = "The vendor grants this read access on its image and signature repositories (in the vendor's project)."
  value       = module.runtime.agent_service_account
}
output "nodes_service_account" {
  description = "The vendor grants this read access on its image repository, so nodes can pull the engine image."
  value       = module.runtime.nodes_service_account
}
output "revoke" {
  description = "How to cut the vendor's access. Data and runtime identities stay."
  value       = "terraform apply -var vendor_access_enabled=false (with your other -var flags), then: kubectl -n ${var.k8s_namespace} scale deployment/memstore-agent --replicas=0"
}
