variable "name" {
  description = "Prefix of every role and of the EKS cluster's name."
  type        = string
}
variable "region" {
  type = string
}
variable "cluster_name" {
  description = "The EKS cluster the vendor creates; only its pods may assume the engine and agent roles."
  type        = string
}
variable "bucket" {
  description = "The memory bucket this module creates (globally unique)."
  type        = string
}
variable "bucket_versioning" {
  type = bool
}
variable "kms_key_arn" {
  description = "A customer-managed KMS key for the bucket; empty for SSE-S3."
  type        = string
}
variable "secret_prefix" {
  description = "Secrets the engine may read start with this."
  type        = string
}
variable "glue_database_prefix" {
  description = "Glue databases (Iceberg namespaces) the engine may create and use start with this."
  type        = string
}
variable "bedrock_model_arns" {
  description = "Bedrock foundation-model and inference-profile ARNs the engine may invoke; empty for none."
  type        = list(string)
}
variable "image_repository_arns" {
  description = "ECR repositories (in the vendor's account) the agent reads image signatures from; empty when the registry isn't ECR."
  type        = list(string)
}
variable "k8s_namespace" {
  type = string
}
variable "engine_k8s_service_account" {
  type = string
}
variable "agent_k8s_service_account" {
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
