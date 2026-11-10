# The vendor management role: assumed from the vendor's account with the external id the vendor issued to this
# customer, and only while var.vendor_access_enabled is true. It can build memstore's network (BYOC only), its EKS
# cluster, node group and add-ons, and write the deployment credential to Secrets Manager. It can't:
#
#   - create, change or attach a policy to any IAM role (it may only pass the roles ../runtime created);
#   - read or write S3 objects, read a secret's value, or touch the memory bucket at all;
#   - change or delete any network resource or cluster that doesn't carry the install's tag;
#   - tag an existing resource to bring it under its control (ec2:CreateTags is allowed only while creating).
#
# Every action is listed with its scope and reason in docs/privileges.md.

data "aws_caller_identity" "this" {}
data "aws_partition" "this" {}

locals {
  account = data.aws_caller_identity.this.account_id
  arn     = "arn:${data.aws_partition.this.partition}"
  ec2     = "${local.arn}:ec2:${var.region}:${local.account}"
  eks     = "${local.arn}:eks:${var.region}:${local.account}"
  tags    = merge(var.tags, { (var.tag_key) = var.tag_value })

  request_tag  = { test = "StringEquals", variable = "aws:RequestTag/${var.tag_key}", values = [var.tag_value] }
  resource_tag = { test = "StringEquals", variable = "aws:ResourceTag/${var.tag_key}", values = [var.tag_value] }

  # BYOC: the resource types the vendor creates, and which existing (tagged) resources each create may use.
  creates = var.create_network ? {
    CreateVpc             = { new = "vpc", uses = [] }
    CreateSubnet          = { new = "subnet", uses = ["vpc"] }
    CreateInternetGateway = { new = "internet-gateway", uses = [] }
    AllocateAddress       = { new = "elastic-ip", uses = [] }
    CreateNatGateway      = { new = "natgateway", uses = ["subnet", "elastic-ip", "vpc"] }
    CreateRouteTable      = { new = "route-table", uses = ["vpc"] }
    CreateSecurityGroup   = { new = "security-group", uses = ["vpc"] }
    } : {
    CreateSecurityGroup = { new = "security-group", uses = [] } # in var.vpc_id, below
  }
  network_types = distinct(concat([for c in local.creates : c.new], flatten([for c in local.creates : c.uses])))

  manage = var.create_network ? [
    "ec2:AssociateRouteTable", "ec2:AttachInternetGateway", "ec2:AuthorizeSecurityGroupEgress",
    "ec2:AuthorizeSecurityGroupIngress", "ec2:CreateRoute", "ec2:DeleteInternetGateway", "ec2:DeleteNatGateway",
    "ec2:DeleteRoute", "ec2:DeleteRouteTable", "ec2:DeleteSecurityGroup", "ec2:DeleteSubnet", "ec2:DeleteVpc",
    "ec2:DetachInternetGateway", "ec2:DisassociateRouteTable", "ec2:ModifySubnetAttribute",
    "ec2:ModifyVpcAttribute", "ec2:ReleaseAddress", "ec2:RevokeSecurityGroupEgress", "ec2:RevokeSecurityGroupIngress",
    ] : [
    "ec2:AuthorizeSecurityGroupEgress", "ec2:AuthorizeSecurityGroupIngress", "ec2:DeleteSecurityGroup",
    "ec2:RevokeSecurityGroupEgress", "ec2:RevokeSecurityGroupIngress",
  ]

  # Describe calls have no resource-level permissions in EC2: they need Resource "*". Listed one by one.
  describe = [
    "ec2:DescribeAccountAttributes", "ec2:DescribeAddresses", "ec2:DescribeAddressesAttribute",
    "ec2:DescribeAvailabilityZones", "ec2:DescribeInternetGateways", "ec2:DescribeNatGateways",
    "ec2:DescribeNetworkAcls", "ec2:DescribeNetworkInterfaces", "ec2:DescribeRouteTables",
    "ec2:DescribeSecurityGroupRules", "ec2:DescribeSecurityGroups", "ec2:DescribeSubnets", "ec2:DescribeTags",
    "ec2:DescribeVpcAttribute", "ec2:DescribeVpcs",
  ]

  public_endpoint = var.allow_public_cluster_endpoint ? [] : [
    { test = "Bool", variable = "eks:endpointPublicAccess", values = ["false"] }
  ]
}

data "aws_iam_policy_document" "trust" {
  statement {
    sid     = "Vendor"
    actions = ["sts:AssumeRole"]
    principals {
      type        = "AWS"
      identifiers = [var.vendor_principal_arn]
    }
    condition {
      test     = "StringEquals"
      variable = "sts:ExternalId"
      values   = [var.external_id]
    }
  }
}

# Removing this role (vendor_access_enabled = false) is the revoke: see the module README.
resource "aws_iam_role" "vendor" {
  count                = var.vendor_access_enabled ? 1 : 0
  name                 = "${var.name}-vendor"
  description          = "Assumed by the memstore vendor to build and upgrade memstore in this account."
  assume_role_policy   = data.aws_iam_policy_document.trust.json
  max_session_duration = 3600
  tags                 = local.tags
}

# --- network ----------------------------------------------------------------------------------------------------

data "aws_iam_policy_document" "network" {
  statement {
    sid       = "Describe"
    actions   = local.describe
    resources = ["*"]
  }

  # Each create, on the resource it makes, only with the install's tag in the request.
  dynamic "statement" {
    for_each = local.creates
    content {
      sid       = "${statement.key}Tagged"
      actions   = ["ec2:${statement.key}"]
      resources = ["${local.ec2}:${statement.value.new}/*"]
      condition {
        test     = local.request_tag.test
        variable = local.request_tag.variable
        values   = local.request_tag.values
      }
    }
  }

  # ...and on what it uses: only resources that already carry the tag (BYOC), or the customer's VPC (BYOVPC).
  dynamic "statement" {
    for_each = { for k, c in local.creates : k => c if length(c.uses) > 0 }
    content {
      sid       = "${statement.key}InTaggedParents"
      actions   = ["ec2:${statement.key}"]
      resources = [for t in statement.value.uses : "${local.ec2}:${t}/*"]
      condition {
        test     = local.resource_tag.test
        variable = local.resource_tag.variable
        values   = local.resource_tag.values
      }
    }
  }
  dynamic "statement" {
    for_each = var.create_network ? [] : [1]
    content {
      sid       = "SecurityGroupsInCustomerVpc"
      actions   = ["ec2:CreateSecurityGroup"]
      resources = ["${local.ec2}:vpc/${var.vpc_id}"]
    }
  }

  statement {
    sid       = "ManageTagged"
    actions   = local.manage
    resources = [for t in local.network_types : "${local.ec2}:${t}/*"]
    condition {
      test     = local.resource_tag.test
      variable = local.resource_tag.variable
      values   = local.resource_tag.values
    }
  }
  # A rule is a new resource on a tagged security group (checked above); rules carry no tag of their own.
  statement {
    sid       = "SecurityGroupRules"
    actions   = ["ec2:AuthorizeSecurityGroupEgress", "ec2:AuthorizeSecurityGroupIngress"]
    resources = ["${local.ec2}:security-group-rule/*"]
  }
  # Tagging only as part of a create above: never an existing resource.
  statement {
    sid       = "TagOnCreate"
    actions   = ["ec2:CreateTags"]
    resources = [for t in local.network_types : "${local.ec2}:${t}/*"]
    condition {
      test     = "StringEquals"
      variable = "ec2:CreateAction"
      values   = keys(local.creates)
    }
  }
}

# --- EKS --------------------------------------------------------------------------------------------------------

data "aws_iam_policy_document" "eks" {
  # CreateCluster has no resource type, so Resource is "*"; the request must carry the install's tag.
  statement {
    sid       = "CreateCluster"
    actions   = ["eks:CreateCluster"]
    resources = ["*"]
    condition {
      test     = local.request_tag.test
      variable = local.request_tag.variable
      values   = local.request_tag.values
    }
    dynamic "condition" {
      for_each = local.public_endpoint
      content {
        test     = condition.value.test
        variable = condition.value.variable
        values   = condition.value.values
      }
    }
  }
  statement {
    sid = "OwnCluster"
    actions = [
      "eks:CreateAddon", "eks:CreateNodegroup", "eks:CreatePodIdentityAssociation", "eks:DeleteCluster",
      "eks:DescribeCluster", "eks:DescribeUpdate", "eks:ListAddons", "eks:ListNodegroups",
      "eks:ListPodIdentityAssociations", "eks:TagResource", "eks:UpdateClusterVersion",
    ]
    resources = ["${local.eks}:cluster/${var.cluster_name}"]
    condition {
      test     = local.resource_tag.test
      variable = local.resource_tag.variable
      values   = local.resource_tag.values
    }
  }
  statement {
    sid       = "OwnClusterConfig"
    actions   = ["eks:UpdateClusterConfig"]
    resources = ["${local.eks}:cluster/${var.cluster_name}"]
    condition {
      test     = local.resource_tag.test
      variable = local.resource_tag.variable
      values   = local.resource_tag.values
    }
    dynamic "condition" {
      for_each = local.public_endpoint
      content {
        test     = condition.value.test
        variable = condition.value.variable
        values   = condition.value.values
      }
    }
  }
  statement {
    sid = "OwnClusterParts"
    actions = [
      "eks:DeleteAddon", "eks:DeleteNodegroup", "eks:DeletePodIdentityAssociation", "eks:DescribeAddon",
      "eks:DescribeNodegroup", "eks:DescribePodIdentityAssociation", "eks:DescribeUpdate", "eks:UpdateAddon",
      "eks:UpdateNodegroupConfig", "eks:UpdateNodegroupVersion",
    ]
    resources = [
      "${local.eks}:addon/${var.cluster_name}/*",
      "${local.eks}:nodegroup/${var.cluster_name}/*",
      "${local.eks}:podidentityassociation/${var.cluster_name}/*",
    ]
  }
  statement {
    sid       = "AddonVersions"
    actions   = ["eks:DescribeAddonVersions"]
    resources = ["*"] # no resource type
  }

  # The roles ../runtime made, to the services that use them, and no other role.
  statement {
    sid       = "PassClusterRole"
    actions   = ["iam:PassRole"]
    resources = [var.cluster_role_arn]
    condition {
      test     = "StringEquals"
      variable = "iam:PassedToService"
      values   = ["eks.amazonaws.com"]
    }
  }
  statement {
    sid       = "PassNodeRole"
    actions   = ["iam:PassRole"]
    resources = [var.nodes_role_arn]
    condition {
      test     = "StringEquals"
      variable = "iam:PassedToService"
      values   = ["eks.amazonaws.com", "ec2.amazonaws.com"]
    }
  }
  statement {
    sid       = "PassPodRoles"
    actions   = ["iam:PassRole"]
    resources = [var.engine_role_arn, var.agent_role_arn]
    condition {
      test     = "StringEquals"
      variable = "iam:PassedToService"
      values   = ["pods.eks.amazonaws.com"]
    }
  }
  statement {
    sid       = "ReadPassableRoles"
    actions   = ["iam:GetRole"]
    resources = [var.cluster_role_arn, var.nodes_role_arn, var.engine_role_arn, var.agent_role_arn]
  }
  statement {
    sid       = "EksServiceLinkedRoles"
    actions   = ["iam:CreateServiceLinkedRole"]
    resources = ["${local.arn}:iam::${local.account}:role/aws-service-role/*"]
    condition {
      test     = "StringEquals"
      variable = "iam:AWSServiceName"
      values   = ["eks.amazonaws.com", "eks-nodegroup.amazonaws.com"]
    }
  }
  dynamic "statement" {
    for_each = var.cluster_kms_key_arn != "" ? [1] : []
    content {
      sid       = "SecretsEncryptionKey"
      actions   = ["kms:CreateGrant", "kms:DescribeKey"]
      resources = [var.cluster_kms_key_arn]
      condition {
        test     = "Bool"
        variable = "kms:GrantIsForAWSResource"
        values   = ["true"]
      }
    }
  }
}

# --- Secrets Manager (write, never read) and the cluster's log group ---------------------------------------------

data "aws_iam_policy_document" "ops" {
  statement {
    sid       = "CreateSecret"
    actions   = ["secretsmanager:CreateSecret", "secretsmanager:TagResource"]
    resources = ["${local.arn}:secretsmanager:${var.region}:${local.account}:secret:${var.secret_prefix}*"]
    condition {
      test     = local.request_tag.test
      variable = local.request_tag.variable
      values   = local.request_tag.values
    }
  }
  statement {
    sid       = "WriteSecret"
    actions   = ["secretsmanager:DescribeSecret", "secretsmanager:PutSecretValue"]
    resources = ["${local.arn}:secretsmanager:${var.region}:${local.account}:secret:${var.secret_prefix}*"]
    condition {
      test     = local.resource_tag.test
      variable = local.resource_tag.variable
      values   = local.resource_tag.values
    }
  }
  statement {
    sid       = "ClusterLogGroup"
    actions   = ["logs:CreateLogGroup", "logs:ListTagsForResource", "logs:PutRetentionPolicy", "logs:TagResource"]
    resources = ["${local.arn}:logs:${var.region}:${local.account}:log-group:/aws/eks/${var.cluster_name}/*"]
  }
  statement {
    sid       = "DescribeLogGroups"
    actions   = ["logs:DescribeLogGroups"]
    resources = ["*"] # no resource type
  }
}

# Managed policies (not inline): each stays under IAM's 6,144-character limit, and all go with the role.
resource "aws_iam_policy" "vendor" {
  for_each = var.vendor_access_enabled ? {
    network = data.aws_iam_policy_document.network.json
    eks     = data.aws_iam_policy_document.eks.json
    ops     = data.aws_iam_policy_document.ops.json
  } : {}
  name        = "${var.name}-vendor-${each.key}"
  description = "memstore vendor management: ${each.key} (docs/privileges.md)"
  policy      = each.value
  tags        = local.tags
}

resource "aws_iam_role_policy_attachment" "vendor" {
  for_each   = aws_iam_policy.vendor
  role       = aws_iam_role.vendor[0].name
  policy_arn = each.value.arn
}
