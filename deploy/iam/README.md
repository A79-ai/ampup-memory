# IAM templates: BYOC and BYOVPC

Terraform the **customer** applies in their own GCP project or AWS account before memstore is installed there. It
creates the identity the vendor manages the install as, the identities memstore runs as, and the bucket (and on
GCP the BigQuery dataset) that holds the memory. Every permission, its scope, its reason and what breaks without
it are in [docs/privileges.md](../../docs/privileges.md).

| | Vendor creates | Customer brings | GCP | AWS |
|---|---|---|---|---|
| BYOC | VPC, cluster, workloads | the project or account | [`gcp/byoc`](gcp/byoc) | [`aws/byoc`](aws/byoc) |
| BYOVPC | cluster, workloads | VPC and subnets | [`gcp/byovpc`](gcp/byovpc) | [`aws/byovpc`](aws/byovpc) |

The installer never creates the bucket or the dataset: `deploy/gcp/terraform` adopts this module's, passed as
`terraform output -raw terraform_vars` (`-var existing_bucket=… -var existing_dataset=…`). See
[Who creates the bucket and the dataset](../../docs/privileges.md#who-creates-the-bucket-and-the-dataset). On GCP
with `cloud_run = true` that output also names this module's accounts
(`-var existing_engine_sa=… -var existing_ingest_sa=…`), and the install then runs on the vendor role alone: it
enables no API, creates no account and makes no grant, and checks this module's instead. (The `init_flags` output
is the same for the hosted service's onboarding wizard, which is not part of this repo.) See
[Installing as the vendor role alone](../../docs/privileges.md#installing-as-the-vendor-role-alone).

`gcp/modules` and `aws/modules` hold the shared parts: `runtime` (workload identities and data stores) and
`vendor` (the management identity; its network permissions differ by variant).

## Revoking the vendor's access

```sh
terraform apply -var vendor_access_enabled=false   # plus the -var flags you installed with
```

GCP: removes the vendor's impersonation grant and disables the management service account. AWS: deletes the
management role and its policies. Data, runtime identities and the running engine stay. To also stop upgrades,
scale the agent to zero (`kubectl -n memstore scale deployment/memstore-agent --replicas=0`); on AWS, delete the
vendor's EKS access entry. Details: [docs/privileges.md](../../docs/privileges.md#revoking-access).

## Checking a change

```sh
terraform fmt -check -recursive deploy/iam
for m in deploy/iam/*/modules/*/ deploy/iam/*/byoc deploy/iam/*/byovpc; do
  terraform -chdir=$m init -backend=false && terraform -chdir=$m validate
  [ ! -d $m/tests ] || terraform -chdir=$m test      # offline plan tests, Terraform >= 1.11
done
deploy/iam/check_permissions.py --gcp-project <a project you can read>   # every action and permission exists
```

CI (`.github/workflows/ci.yml`, job `iam-templates`) runs all of it except the GCP half of the permission check,
which needs a gcloud login. None of it touches a cloud account. When you add a permission, add its row to
docs/privileges.md.
