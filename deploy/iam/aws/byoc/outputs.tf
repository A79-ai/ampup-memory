output "vendor_role_arn" {
  description = "Give this to the vendor: the role it assumes (with your external id) to install and upgrade memstore."
  value       = module.vendor.role_arn
}
output "vendor_access_enabled" {
  value = module.vendor.enabled
}
output "install_tag" {
  description = "Every resource the vendor creates must carry this tag, and it may change only resources that do."
  value       = { (var.tag_key) = var.name }
}
output "cluster_name" {
  value = var.name
}
output "bucket" {
  value = module.runtime.bucket
}
output "engine_role_arn" {
  value = module.runtime.engine_role_arn
}
output "agent_role_arn" {
  value = module.runtime.agent_role_arn
}
output "cluster_role_arn" {
  value = module.runtime.cluster_role_arn
}
output "nodes_role_arn" {
  value = module.runtime.nodes_role_arn
}
output "revoke" {
  description = "How to cut the vendor's access. Data and runtime roles stay."
  value       = "terraform apply -var vendor_access_enabled=false (with your other -var flags); aws eks delete-access-entry --cluster-name ${var.name} --principal-arn <the vendor role ARN, from vendor_role_arn before the apply>; kubectl -n ${var.k8s_namespace} scale deployment/memstore-agent --replicas=0"
}
