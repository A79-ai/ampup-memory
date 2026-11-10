# AWS BYOC

The vendor builds memstore's VPC and EKS cluster in your account and runs the engine there, as a role this
module creates. The vendor assumes it from its own account with an external id it issued to you. You apply this
once, as an account administrator; the vendor's role can't change any of it. Permissions:
[docs/privileges.md](../../../../docs/privileges.md#aws).

```sh
cd deploy/iam/aws/byoc
terraform init
export TF_VAR_external_id=<the external id the vendor gave you>   # sensitive: kept out of plan output
terraform apply \
  -var region=us-east-1 \
  -var bucket=acme-memstore-memory \
  -var vendor_principal_arn=arn:aws:iam::<vendor account>:role/memstore-deployer
terraform output vendor_role_arn   # send to the vendor
```

Everything the vendor creates must carry the tag `memstore-install = <name>`, and it may change or delete only
resources that do. The cluster is named `<name>` (default `memstore`); only that cluster's `memstore/memstore` and
`memstore/memstore-agent` pods can assume the engine and agent roles (EKS Pod Identity).

| Variable | Default | |
|---|---|---|
| `bucket`, `vendor_principal_arn`, `external_id` | required | `external_id` is at least 16 characters; it is stored in Terraform state |
| `region` | `us-east-1` | |
| `name` | `memstore` | Prefix of roles and policies, the cluster's name and the tag's value |
| `vendor_access_enabled` | `true` | `false` revokes the vendor's access |
| `allow_public_cluster_endpoint` | `true` | `false`: the vendor can only run the cluster with a private API endpoint |
| `kms_key_arn`, `cluster_kms_key_arn` | none | Your KMS keys for the bucket and for EKS Secrets |
| `bedrock_model_arns` | none | Models the engine may invoke |
| `image_repository_arns` | none | The vendor's ECR repositories the agent reads signatures from |
| `secret_prefix`, `glue_database_prefix` | `memstore-`, `memstore` | Secrets and Glue databases the engine may use |

**Revoke:**

```sh
terraform apply ... -var vendor_access_enabled=false
aws eks delete-access-entry --cluster-name memstore --principal-arn <vendor role ARN>
kubectl -n memstore scale deployment/memstore-agent --replicas=0   # also stop upgrades
```

The bucket and the runtime roles stay, and the engine keeps running. See
[Revoking access](../../../../docs/privileges.md#revoking-access).
