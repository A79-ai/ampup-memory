variable "name" {
  type = string
}
variable "region" {
  type = string
}
variable "vendor_principal_arn" {
  description = "The vendor's IAM role allowed to assume the management role."
  type        = string
}
variable "external_id" {
  description = "The external id the vendor issued to this customer; the vendor must present it to assume the role."
  type        = string
  sensitive   = true
}
variable "vendor_access_enabled" {
  description = "false deletes the management role and its policies."
  type        = bool
}
variable "create_network" {
  description = "BYOC: the vendor creates its own VPC. BYOVPC: false, and vpc_id names the customer's."
  type        = bool
}
variable "vpc_id" {
  description = "BYOVPC: the customer's VPC; the vendor may only add security groups to it."
  type        = string
  default     = null
}
variable "cluster_name" {
  type = string
}
variable "allow_public_cluster_endpoint" {
  description = "false: the vendor may only create or configure the cluster with its public API endpoint off."
  type        = bool
}
variable "cluster_kms_key_arn" {
  description = "A customer-managed KMS key for EKS secrets encryption; empty for none."
  type        = string
}
variable "cluster_role_arn" {
  type = string
}
variable "nodes_role_arn" {
  type = string
}
variable "engine_role_arn" {
  type = string
}
variable "agent_role_arn" {
  type = string
}
variable "secret_prefix" {
  type = string
}
variable "tag_key" {
  type = string
}
variable "tag_value" {
  type = string
}
variable "tags" {
  type = map(string)
}
