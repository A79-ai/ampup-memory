variable "region" {
  type    = string
  default = "us-east-1"
}
variable "name" {
  description = "Prefix of every role and policy, and the name of the EKS cluster the vendor creates."
  type        = string
  default     = "memstore"
  validation {
    condition     = can(regex("^[a-z][a-z0-9-]{2,30}$", var.name))
    error_message = "name is 3 to 31 lowercase letters, digits and dashes, starting with a letter."
  }
}
variable "bucket" {
  description = "The memory bucket this module creates and the customer owns (globally unique)."
  type        = string
}
variable "bucket_versioning" {
  description = "Keep overwritten and deleted objects as noncurrent versions (recoverable, billed)."
  type        = bool
  default     = true
}
variable "kms_key_arn" {
  description = "A customer-managed KMS key for the bucket; empty for SSE-S3."
  type        = string
  default     = ""
}
variable "vendor_principal_arn" {
  description = "The vendor's IAM role that may assume the management role, e.g. arn:aws:iam::<vendor account>:role/memstore-deployer."
  type        = string
  validation {
    condition     = can(regex("^arn:aws[a-z-]*:iam::[0-9]{12}:role/", var.vendor_principal_arn))
    error_message = "vendor_principal_arn is an IAM role ARN."
  }
}
variable "external_id" {
  description = "The external id the vendor issued to you. Kept out of plan output; it is stored in Terraform state."
  type        = string
  sensitive   = true
  validation {
    condition     = length(var.external_id) >= 16
    error_message = "external_id is at least 16 characters."
  }
}
variable "vendor_access_enabled" {
  description = "Set false to revoke the vendor's access (deletes the management role). Runtime roles and data are kept."
  type        = bool
  default     = true
}
variable "allow_public_cluster_endpoint" {
  description = "false: the vendor may only run the cluster with its public API endpoint off (it then needs private connectivity)."
  type        = bool
  default     = true
}
variable "cluster_kms_key_arn" {
  description = "A customer-managed KMS key the vendor may use for EKS secrets encryption; empty for none."
  type        = string
  default     = ""
}
variable "secret_prefix" {
  description = "Secrets the engine may read and the vendor may write start with this."
  type        = string
  default     = "memstore-"
}
variable "glue_database_prefix" {
  description = "Glue databases (Iceberg namespaces) the engine may create and use start with this."
  type        = string
  default     = "memstore"
}
variable "bedrock_model_arns" {
  description = "Bedrock foundation-model and inference-profile ARNs the engine may invoke; empty for none."
  type        = list(string)
  default     = []
}
variable "image_repository_arns" {
  description = "The vendor's ECR repositories the agent reads image signatures from; empty when the registry isn't ECR."
  type        = list(string)
  default     = []
}
variable "k8s_namespace" {
  type    = string
  default = "memstore"
}
variable "engine_k8s_service_account" {
  type    = string
  default = "memstore"
}
variable "agent_k8s_service_account" {
  type    = string
  default = "memstore-agent"
}
variable "tag_key" {
  description = "The tag every vendor-created resource must carry, and the only resources the vendor may change."
  type        = string
  default     = "memstore-install"
}
variable "tags" {
  description = "Extra tags for the resources this module creates."
  type        = map(string)
  default     = {}
}
variable "vpc_id" {
  description = "The customer's VPC memstore runs in. The vendor may add tagged security groups to it and nothing else."
  type        = string
}
variable "subnet_ids" {
  description = "The customer's private subnets for the cluster and its nodes (passed on to the vendor; see README.md on why IAM can't enforce them)."
  type        = list(string)
}
