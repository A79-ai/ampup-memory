output "role_arn" {
  description = "The management role the vendor assumes (null while vendor access is off)."
  value       = var.vendor_access_enabled ? aws_iam_role.vendor[0].arn : null
}
output "enabled" {
  value = var.vendor_access_enabled
}
output "trust_policy" {
  description = "Who may assume the management role (for review and tests). Holds the external id."
  value       = data.aws_iam_policy_document.trust.json
  sensitive   = true
}
output "policies" {
  description = "The management role's policies by name (empty while vendor access is off)."
  value       = { for k, p in aws_iam_policy.vendor : k => p.policy }
}
