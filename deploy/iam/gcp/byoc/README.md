# GCP BYOC

The vendor builds memstore's VPC, GKE cluster (or Cloud Run services) and workloads in your project, as a
service account this module creates. You apply this once, as a project owner; the vendor's account can't change
any of it. Permissions: [docs/privileges.md](../../../../docs/privileges.md#gcp).

```sh
cd deploy/iam/gcp/byoc
terraform init
terraform apply \
  -var project=acme-memstore \
  -var bucket=acme-memstore-memory \
  -var vendor_principal=serviceAccount:deployer@<vendor project>.iam.gserviceaccount.com \
  -var dedicated_project=true
terraform output vendor_service_account   # send to the vendor
terraform output agent_service_account    # the vendor lets it read image signatures
terraform output nodes_service_account    # the vendor lets it pull the engine image
terraform output -raw terraform_vars      # for deploy/gcp/terraform: -var existing_bucket=… -var existing_dataset=…
```

The bucket (and with `cloud_run = true` the `<name>_api` dataset) is made here and stays yours; the vendor adopts
it by name and can't create, change or delete it. See
[Who creates the bucket and the dataset](../../../../docs/privileges.md#who-creates-the-bucket-and-the-dataset).

**Use a project for memstore alone.** Workload Identity trusts the engine's Kubernetes service account
(`memstore/memstore`) in every GKE cluster of the project, so anyone who can create a cluster there could run a pod
as the engine. The module refuses to plan until you set `dedicated_project = true`, confirming no one else can
create GKE clusters or Cloud Run services in the project. The vendor itself can create only the cluster named
`<name>`. See [Workload Identity and the dedicated project](../../../../docs/privileges.md#workload-identity-and-the-dedicated-project).

Once the vendor has created the GKE cluster, bind the engine and agent pods to their accounts (Workload Identity
names the cluster's identity pool, which exists only then):

```sh
terraform apply ... -var gke_workload_identity_ready=true
```

For the Cloud Run engine with the BigQuery surface and scheduled ingest (what `deploy/gcp/terraform` builds), add
`-var cloud_run=true` (and `-var gke=false` if there is no cluster). Cloud Run has no IAM conditions, so its
grants are made on the engine service and the load and ingest jobs themselves: once the vendor has created them
(`<name>-engine`, `<name>-load`, `<name>-ingest`) and the BigQuery connection, apply again with
`-var cloud_run_deployed=true -var bigquery_connection_service_account=<the connection's service account>`
(`terraform output after_vendor_deploys`). Until then the engine refuses callers. For an install
that runs no scheduled ingest, apply with `-var scheduled_ingest=false`.

With `cloud_run = true`, `terraform_vars` lets the vendor install on its role alone (no API enabled, no account
created, no grant made; each checked instead). The org is then yours to create
(`memstore org create <org> --admin <you> --direct`). The `init_flags` output carries the same for the hosted
service's onboarding wizard, which is not part of this repo. See
[Installing as the vendor role alone](../../../../docs/privileges.md#installing-as-the-vendor-role-alone).

| Variable | Default | |
|---|---|---|
| `project`, `bucket`, `vendor_principal` | required | `vendor_principal` is an IAM member: `serviceAccount:…`, `principal://…` or `principalSet://…` (Workload Identity Federation) |
| `region` | `us-central1` | |
| `name` | `memstore` | Prefix of every resource; grants are narrowed to it |
| `dedicated_project` | `false` | Must be `true`: the project is memstore's alone |
| `vendor_access_enabled` | `true` | `false` revokes the vendor's access |
| `gke_workload_identity_ready`, `cloud_run_deployed` | `false` | Second apply, once the cluster or the Cloud Run resources exist |
| `gke`, `cloud_run` | `true`, `false` | Which runtime to grant for |
| `scheduled_ingest` | `true` | Cloud Run: the ingest account and job grants, and the vendor's Cloud Scheduler and `_bin/` rights. `false` for an install that runs no scheduled ingest |
| `secret_prefix` | `memstore-` | Secrets the engine reads and the vendor writes |
| `bucket_versioning` | `true` | |
| `k8s_namespace`, `engine_k8s_service_account`, `agent_k8s_service_account` | `memstore`, `memstore`, `memstore-agent` | Must match the manifests |

**Revoke:** `terraform apply ... -var vendor_access_enabled=false`. The bucket, dataset and runtime accounts stay.
See [Revoking access](../../../../docs/privileges.md#revoking-access).

GCP keeps a deleted custom role id reserved for up to 37 days (`memstore_vendor_base`, …); to reinstall sooner, use
another `name`.
