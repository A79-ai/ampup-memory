# GCP BYOVPC

You bring the VPC and one subnet (in this project or a Shared VPC host project); the vendor builds memstore's GKE
cluster (or Cloud Run services) inside it, as a service account this module creates. The vendor holds no
permission to create or change any network resource: only `roles/compute.networkUser` on your one subnet.
Permissions: [docs/privileges.md](../../../../docs/privileges.md#gcp).

```sh
cd deploy/iam/gcp/byovpc
terraform init
terraform apply \
  -var project=acme-memstore \
  -var bucket=acme-memstore-memory \
  -var vendor_principal=serviceAccount:deployer@<vendor project>.iam.gserviceaccount.com \
  -var network_project=acme-network-host \
  -var subnetwork=memstore-nodes \
  -var dedicated_project=true
terraform output vendor_service_account   # send to the vendor, with the subnet's name and its secondary ranges
terraform output -raw terraform_vars      # and this: the bucket and dataset made here, for deploy/gcp/terraform to adopt
```

For GKE the subnet needs secondary ranges for pods and services, and Cloud NAT (or another egress) so private
nodes reach the control plane and the vendor's registry. With a Shared VPC (`network_project` is another
project), the module also grants Google's GKE, Cloud Run and Google APIs service agents what Shared VPC needs on
the subnet and host project; you need permission to set IAM there.

The service project must be memstore's alone (`dedicated_project = true`; the network can stay in a shared host
project): Workload Identity trusts the engine's Kubernetes service account in every GKE cluster of the service
project. See [`../byoc`](../byoc/README.md) and
[docs/privileges.md](../../../../docs/privileges.md#workload-identity-and-the-dedicated-project).

After the cluster exists, `terraform apply ... -var gke_workload_identity_ready=true`. Other variables
(`cloud_run`, `cloud_run_deployed`, `scheduled_ingest`) and installing as the vendor role alone are as in
[`../byoc`](../byoc/README.md).

**Revoke:** `terraform apply ... -var vendor_access_enabled=false`. Nothing in your network changes, and the
bucket, dataset and runtime accounts stay.
