# AWS BYOVPC

You bring the VPC and private subnets; the vendor builds memstore's EKS cluster inside them, as a role this module
creates. The vendor may add security groups (tagged `memstore-install = <name>`) to your VPC and change only
those: no VPC, subnet, route, gateway or address permission. Permissions:
[docs/privileges.md](../../../../docs/privileges.md#aws).

```sh
cd deploy/iam/aws/byovpc
terraform init
export TF_VAR_external_id=<the external id the vendor gave you>
terraform apply \
  -var region=us-east-1 \
  -var bucket=acme-memstore-memory \
  -var vendor_principal_arn=arn:aws:iam::<vendor account>:role/memstore-deployer \
  -var vpc_id=vpc-0abc… \
  -var 'subnet_ids=["subnet-0aaa…","subnet-0bbb…"]'
terraform output vendor_role_arn   # send to the vendor, with subnet_ids
```

The subnets need egress (NAT or VPC endpoints) to the EKS control plane, the vendor's registry and the control
plane. IAM can't hold `eks:CreateCluster` to particular subnets (EKS has no condition key for them), so
`subnet_ids` is what the vendor is told to use, and CloudTrail shows what it did use.

Other variables, and the revoke, are as in [`../byoc`](../byoc/README.md).
