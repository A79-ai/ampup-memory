# Plans the module offline (no AWS credentials; nothing is created) and checks the promises docs/privileges.md
# makes about the vendor role. Run: terraform init -backend=false && terraform test   (Terraform >= 1.7)

provider "aws" {
  region                      = "us-east-1"
  access_key                  = "offline"
  secret_key                  = "offline"
  skip_credentials_validation = true
  skip_requesting_account_id  = true
  skip_metadata_api_check     = true
  skip_region_validation      = true
}

override_data {
  target = module.runtime.data.aws_caller_identity.this
  values = { account_id = "111111111111" }
}
override_data {
  target = module.vendor.data.aws_caller_identity.this
  values = { account_id = "111111111111" }
}
override_resource {
  target          = module.runtime.aws_iam_role.engine
  override_during = plan
  values          = { arn = "arn:aws:iam::111111111111:role/memstore-engine" }
}
override_resource {
  target          = module.runtime.aws_iam_role.agent
  override_during = plan
  values          = { arn = "arn:aws:iam::111111111111:role/memstore-agent" }
}
override_resource {
  target          = module.runtime.aws_iam_role.cluster
  override_during = plan
  values          = { arn = "arn:aws:iam::111111111111:role/memstore-cluster" }
}
override_resource {
  target          = module.runtime.aws_iam_role.nodes
  override_during = plan
  values          = { arn = "arn:aws:iam::111111111111:role/memstore-nodes" }
}

variables {
  bucket               = "example-memstore-data"
  vendor_principal_arn = "arn:aws:iam::222222222222:role/memstore-deployer"
  external_id          = "offline-test-external-id-0000"
  bedrock_model_arns   = ["arn:aws:bedrock:us-east-1::foundation-model/anthropic.claude-haiku-4-5-20251001-v1:0"]
  vpc_id               = "vpc-0123456789abcdef0"
  subnet_ids           = ["subnet-0123456789abcdef0", "subnet-0123456789abcdef1"]
}

run "vendor_role_is_least_privilege" {
  command = plan

  assert {
    condition     = jsondecode(module.vendor.trust_policy).Statement[0].Condition.StringEquals["sts:ExternalId"] == var.external_id
    error_message = "the vendor role must require the external id"
  }

  # No action wildcards, and Resource "*" only for actions that have no resource type.
  assert {
    condition = alltrue(flatten([
      for doc in values(module.vendor.policies) : [
        for s in jsondecode(doc).Statement : [
          for a in flatten([s.Action]) : !strcontains(a, "*")
        ]
      ]
    ]))
    error_message = "a vendor policy has a wildcard action"
  }
  assert {
    condition = alltrue(flatten([
      for doc in values(module.vendor.policies) : [
        for s in jsondecode(doc).Statement : [
          for a in flatten([s.Action]) : contains(flatten([s.Resource]), "*") ? (
            startswith(a, "ec2:Describe") || contains(["eks:CreateCluster", "eks:DescribeAddonVersions", "logs:DescribeLogGroups"], a)
          ) : true
        ]
      ]
    ]))
    error_message = "a vendor action other than the documented ones has Resource \"*\""
  }

  # No data access and no IAM administration.
  assert {
    condition = length(setintersection(
      toset(flatten([for doc in values(module.vendor.policies) : [for s in jsondecode(doc).Statement : flatten([s.Action])]])),
      toset([
        "s3:GetObject", "s3:PutObject", "s3:DeleteObject", "s3:ListBucket", "s3:DeleteBucket", "s3:PutBucketPolicy",
        "secretsmanager:GetSecretValue", "secretsmanager:DeleteSecret", "glue:GetTable", "glue:DeleteDatabase",
        "iam:CreateRole", "iam:PutRolePolicy", "iam:AttachRolePolicy", "iam:UpdateAssumeRolePolicy",
        "iam:CreatePolicyVersion", "iam:CreateUser", "iam:CreateAccessKey", "eks:CreateAccessEntry",
        "eks:AssociateAccessPolicy", "kms:Decrypt",
      ])
    )) == 0
    error_message = "the vendor role can read data or administer IAM"
  }

  # Tags only at creation, so the vendor can't adopt a customer's resource by tagging it.
  assert {
    condition = alltrue([
      for s in jsondecode(module.vendor.policies.network).Statement :
      contains(flatten([s.Action]), "ec2:CreateTags") ? can(s.Condition.StringEquals["ec2:CreateAction"]) : true
    ])
    error_message = "ec2:CreateTags must be limited to tag-on-create"
  }

  # PassRole names exactly the four runtime roles, each with iam:PassedToService.
  assert {
    condition = alltrue([
      for s in jsondecode(module.vendor.policies.eks).Statement :
      contains(flatten([s.Action]), "iam:PassRole") ? (
        can(s.Condition.StringEquals["iam:PassedToService"]) && alltrue([for r in flatten([s.Resource]) : startswith(r, "arn:aws:iam::111111111111:role/memstore-")])
      ) : true
    ])
    error_message = "iam:PassRole must be limited to the runtime roles and their services"
  }

  assert {
    condition     = alltrue([for doc in values(module.vendor.policies) : length(replace(doc, "/\\s/", "")) <= 6144])
    error_message = "a vendor policy is over IAM's 6,144-character limit for a managed policy"
  }
}

run "no_network_creation" {
  command = plan

  assert {
    condition = length(setintersection(
      toset(flatten([for doc in values(module.vendor.policies) : [for s in jsondecode(doc).Statement : flatten([s.Action])]])),
      toset([
        "ec2:CreateVpc", "ec2:CreateSubnet", "ec2:CreateInternetGateway", "ec2:CreateNatGateway", "ec2:AllocateAddress",
        "ec2:CreateRouteTable", "ec2:CreateRoute", "ec2:AssociateRouteTable", "ec2:ModifyVpcAttribute",
        "ec2:ModifySubnetAttribute", "ec2:DeleteVpc", "ec2:DeleteSubnet", "ec2:AttachInternetGateway",
      ])
    )) == 0
    error_message = "in BYOVPC the vendor may not create or change network resources"
  }
  assert {
    condition = alltrue([
      for s in jsondecode(module.vendor.policies.network).Statement :
      contains(flatten([s.Action]), "ec2:CreateSecurityGroup") ? alltrue([
        for r in flatten([s.Resource]) : endswith(r, ":vpc/vpc-0123456789abcdef0") || endswith(r, ":security-group/*")
      ]) : true
    ])
    error_message = "security groups may only be created in the customer's VPC"
  }
}

run "engine_role_is_scoped" {
  command = plan

  assert {
    condition = alltrue([
      for s in jsondecode(module.runtime.engine_policy).Statement : alltrue([for r in flatten([s.Resource]) : r != "*"])
    ])
    error_message = "the engine role has a Resource \"*\" statement"
  }
  assert {
    condition     = length(jsondecode(module.runtime.trust_policies.engine).Statement) == 2
    error_message = "the engine role trusts only EKS Pod Identity, with its session tags checked"
  }
}
