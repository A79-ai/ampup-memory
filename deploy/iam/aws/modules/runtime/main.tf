# The identities memstore runs as in the customer's AWS account, and the bucket it keeps the memory in. The
# customer applies this (through ../../byoc or ../../byovpc), never the vendor: the vendor's role can pass these
# roles to EKS but can't create, change or attach policies to any role. Each action is listed with its reason in
# docs/privileges.md.
#
# The bucket is created here, so the customer owns it outright: revoking the vendor's access, or destroying the
# vendor's cluster, leaves the data where it is. The vendor's role has no S3 data access at all.

data "aws_caller_identity" "this" {}
data "aws_partition" "this" {}

locals {
  account   = data.aws_caller_identity.this.account_id
  partition = data.aws_partition.this.partition
  region    = var.region
  bucket    = "arn:${local.partition}:s3:::${var.bucket}"
  glue      = "arn:${local.partition}:glue:${local.region}:${local.account}"
  cluster   = "arn:${local.partition}:eks:${local.region}:${local.account}:cluster/${var.cluster_name}"
  managed   = "arn:${local.partition}:iam::aws:policy"
  tags      = merge(var.tags, { (var.tag_key) = var.tag_value })
}

# --- the memory bucket (customer-owned) -------------------------------------------------------------------------

resource "aws_s3_bucket" "data" {
  bucket        = var.bucket
  force_destroy = false # never takes the memory with it
  tags          = local.tags
}

resource "aws_s3_bucket_public_access_block" "data" {
  bucket                  = aws_s3_bucket.data.id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_ownership_controls" "data" {
  bucket = aws_s3_bucket.data.id
  rule {
    object_ownership = "BucketOwnerEnforced"
  }
}

resource "aws_s3_bucket_versioning" "data" {
  bucket = aws_s3_bucket.data.id
  versioning_configuration {
    status = var.bucket_versioning ? "Enabled" : "Suspended"
  }
}

resource "aws_s3_bucket_server_side_encryption_configuration" "data" {
  bucket = aws_s3_bucket.data.id
  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm     = var.kms_key_arn == "" ? "AES256" : "aws:kms"
      kms_master_key_id = var.kms_key_arn == "" ? null : var.kms_key_arn
    }
    bucket_key_enabled = var.kms_key_arn != ""
  }
}

resource "aws_s3_bucket_policy" "data" {
  bucket = aws_s3_bucket.data.id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [{
      Sid       = "TLSOnly"
      Effect    = "Deny"
      Principal = "*"
      Action    = "s3:*" # a deny: every S3 action over plain HTTP is refused
      Resource  = [local.bucket, "${local.bucket}/*"]
      Condition = { Bool = { "aws:SecureTransport" = "false" } }
    }]
  })
  depends_on = [aws_s3_bucket_public_access_block.data]
}

# --- trust: EKS Pod Identity, for one Kubernetes service account in the vendor's cluster -----------------------

# Pod Identity assumes the role with session tags naming the cluster, namespace and service account. The
# TagSession statement requires them to be ours, so no other pod (or cluster) gets these credentials even if a
# pod identity association names the role.
data "aws_iam_policy_document" "trust" {
  for_each = {
    engine = var.engine_k8s_service_account
    agent  = var.agent_k8s_service_account
  }
  statement {
    sid     = "PodIdentity"
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["pods.eks.amazonaws.com"]
    }
    condition {
      test     = "StringEquals"
      variable = "aws:SourceAccount"
      values   = [local.account]
    }
  }
  statement {
    sid     = "PodIdentityTags"
    actions = ["sts:TagSession"]
    principals {
      type        = "Service"
      identifiers = ["pods.eks.amazonaws.com"]
    }
    condition {
      test     = "StringEquals"
      variable = "aws:RequestTag/eks-cluster-arn"
      values   = [local.cluster]
    }
    condition {
      test     = "StringEquals"
      variable = "aws:RequestTag/kubernetes-namespace"
      values   = [var.k8s_namespace]
    }
    condition {
      test     = "StringEquals"
      variable = "aws:RequestTag/kubernetes-service-account"
      values   = [each.value]
    }
  }
}

# --- engine -----------------------------------------------------------------------------------------------------

resource "aws_iam_role" "engine" {
  name                 = "${var.name}-engine"
  description          = "memstore engine: the memory bucket, its Glue databases, its secrets, Bedrock models."
  assume_role_policy   = data.aws_iam_policy_document.trust["engine"].json
  max_session_duration = 3600
  tags                 = local.tags
}

data "aws_iam_policy_document" "engine" {
  statement {
    sid       = "ListBucket"
    actions   = ["s3:ListBucket"]
    resources = [local.bucket]
  }
  statement {
    sid       = "Objects"
    actions   = ["s3:AbortMultipartUpload", "s3:DeleteObject", "s3:GetObject", "s3:PutObject"]
    resources = ["${local.bucket}/*"]
  }
  statement {
    sid = "IcebergCatalog"
    actions = [
      "glue:CreateDatabase", "glue:DeleteDatabase", "glue:GetDatabase", "glue:GetDatabases", "glue:UpdateDatabase",
      "glue:CreateTable", "glue:DeleteTable", "glue:GetTable", "glue:GetTables", "glue:UpdateTable",
    ]
    resources = [
      "${local.glue}:catalog",
      "${local.glue}:database/${var.glue_database_prefix}*",
      "${local.glue}:table/${var.glue_database_prefix}*/*",
    ]
  }
  statement {
    sid       = "ReadOwnSecrets"
    actions   = ["secretsmanager:DescribeSecret", "secretsmanager:GetSecretValue"]
    resources = ["arn:${local.partition}:secretsmanager:${local.region}:${local.account}:secret:${var.secret_prefix}*"]
  }
  dynamic "statement" {
    for_each = length(var.bedrock_model_arns) > 0 ? [1] : []
    content {
      sid       = "Bedrock"
      actions   = ["bedrock:InvokeModel", "bedrock:InvokeModelWithResponseStream"]
      resources = var.bedrock_model_arns
    }
  }
  dynamic "statement" {
    for_each = var.kms_key_arn != "" ? [1] : []
    content {
      sid       = "BucketKey"
      actions   = ["kms:Decrypt", "kms:GenerateDataKey"]
      resources = [var.kms_key_arn]
      condition {
        test     = "StringEquals"
        variable = "kms:ViaService"
        values   = ["s3.${local.region}.amazonaws.com"]
      }
    }
  }
}

resource "aws_iam_role_policy" "engine" {
  name   = "memstore-engine"
  role   = aws_iam_role.engine.id
  policy = data.aws_iam_policy_document.engine.json
}

# --- agent ------------------------------------------------------------------------------------------------------

# The agent reads image signatures from the vendor's registry and talks to the control plane over HTTPS. In this
# account it needs nothing, unless the images are in ECR (then: pull from those repositories only).
resource "aws_iam_role" "agent" {
  name                 = "${var.name}-agent"
  description          = "memstore deployment agent: pulls image signatures from the vendor's ECR repositories, nothing else."
  assume_role_policy   = data.aws_iam_policy_document.trust["agent"].json
  max_session_duration = 3600
  tags                 = local.tags
}

data "aws_iam_policy_document" "agent" {
  count = length(var.image_repository_arns) > 0 ? 1 : 0
  statement {
    sid       = "EcrToken"
    actions   = ["ecr:GetAuthorizationToken"]
    resources = ["*"] # ecr:GetAuthorizationToken has no resource type (docs/privileges.md)
  }
  statement {
    sid       = "ReadImages"
    actions   = ["ecr:BatchCheckLayerAvailability", "ecr:BatchGetImage", "ecr:GetDownloadUrlForLayer"]
    resources = var.image_repository_arns
  }
}

resource "aws_iam_role_policy" "agent" {
  count  = length(var.image_repository_arns) > 0 ? 1 : 0
  name   = "memstore-agent"
  role   = aws_iam_role.agent.id
  policy = data.aws_iam_policy_document.agent[0].json
}

# --- EKS cluster and node roles (AWS managed policies, the documented minimum for EKS) -------------------------

data "aws_iam_policy_document" "eks_trust" {
  statement {
    actions = ["sts:AssumeRole", "sts:TagSession"]
    principals {
      type        = "Service"
      identifiers = ["eks.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "cluster" {
  name               = "${var.name}-cluster"
  description        = "The EKS control plane of memstore's cluster."
  assume_role_policy = data.aws_iam_policy_document.eks_trust.json
  tags               = local.tags
}

resource "aws_iam_role_policy_attachment" "cluster" {
  role       = aws_iam_role.cluster.name
  policy_arn = "${local.managed}/AmazonEKSClusterPolicy"
}

data "aws_iam_policy_document" "node_trust" {
  statement {
    actions = ["sts:AssumeRole"]
    principals {
      type        = "Service"
      identifiers = ["ec2.amazonaws.com"]
    }
  }
}

resource "aws_iam_role" "nodes" {
  name               = "${var.name}-nodes"
  description        = "The EC2 nodes of memstore's EKS cluster."
  assume_role_policy = data.aws_iam_policy_document.node_trust.json
  tags               = local.tags
}

resource "aws_iam_role_policy_attachment" "nodes" {
  for_each   = toset(["AmazonEKSWorkerNodePolicy", "AmazonEKS_CNI_Policy", "AmazonEC2ContainerRegistryPullOnly"])
  role       = aws_iam_role.nodes.name
  policy_arn = "${local.managed}/${each.key}"
}
