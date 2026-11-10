# BYOC on AWS: the vendor builds memstore's VPC and its EKS cluster in the customer's account, as a management role
# this module creates, and runs the engine there. The customer applies this module once; the vendor never holds a
# permission to change it. See README.md and docs/privileges.md.

module "runtime" {
  source = "../modules/runtime"

  name                       = var.name
  region                     = var.region
  cluster_name               = var.name
  bucket                     = var.bucket
  bucket_versioning          = var.bucket_versioning
  kms_key_arn                = var.kms_key_arn
  secret_prefix              = var.secret_prefix
  glue_database_prefix       = var.glue_database_prefix
  bedrock_model_arns         = var.bedrock_model_arns
  image_repository_arns      = var.image_repository_arns
  k8s_namespace              = var.k8s_namespace
  engine_k8s_service_account = var.engine_k8s_service_account
  agent_k8s_service_account  = var.agent_k8s_service_account
  tag_key                    = var.tag_key
  tag_value                  = var.name
  tags                       = var.tags
}

module "vendor" {
  source = "../modules/vendor"

  name                          = var.name
  region                        = var.region
  vendor_principal_arn          = var.vendor_principal_arn
  external_id                   = var.external_id
  vendor_access_enabled         = var.vendor_access_enabled
  cluster_name                  = var.name
  allow_public_cluster_endpoint = var.allow_public_cluster_endpoint
  cluster_kms_key_arn           = var.cluster_kms_key_arn
  cluster_role_arn              = module.runtime.cluster_role_arn
  nodes_role_arn                = module.runtime.nodes_role_arn
  engine_role_arn               = module.runtime.engine_role_arn
  agent_role_arn                = module.runtime.agent_role_arn
  secret_prefix                 = var.secret_prefix
  tag_key                       = var.tag_key
  tag_value                     = var.name
  tags                          = var.tags

  create_network = true
}
