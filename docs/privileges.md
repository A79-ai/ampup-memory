# Privilege reference: BYOC and BYOVPC

Every permission AmpUp Memory holds in a customer's GCP project or AWS account, who holds it, on what, why, and
what stops working if the customer takes it away. The Terraform that grants them is in
[`deploy/iam`](../deploy/iam): one module per cloud and variant.

| | The vendor creates | The customer brings | Module |
|---|---|---|---|
| **BYOC** | the VPC, the cluster (GKE or EKS) and the workloads | the project or account | `deploy/iam/gcp/byoc`, `deploy/iam/aws/byoc` |
| **BYOVPC** | the cluster and the workloads, inside the customer's network | the VPC and subnets (GCP: one subnet, optionally in a Shared VPC host project) | `deploy/iam/gcp/byovpc`, `deploy/iam/aws/byovpc` |

In both variants the customer applies the module, and the module creates every identity, every grant and the
data stores. The vendor's role never holds an IAM-admin permission, so it can't widen its own access or anyone
else's.

## Principles

1. **Two kinds of identity.** The *vendor management* identity is what the vendor's people and automation act
   as to install and upgrade. The *runtime* identities (engine, ingest, agent, nodes) are what AmpUp Memory runs as.
   Revoking the first leaves the second, so the customer's AmpUp Memory keeps working without the vendor.
2. **The customer owns the data.** The memory bucket (and on GCP the BigQuery dataset) is created by the
   customer's module with `force_destroy = false` / `delete_contents_on_destroy = false`. The vendor can't read
   or write its objects, read a secret's value, read table data, or delete a data store.
3. **No IAM administration by the vendor.** No `setIamPolicy` on GCP; no `iam:CreateRole`, `iam:PutRolePolicy`,
   `iam:AttachRolePolicy` on AWS. The vendor attaches the runtime identities to workloads (`iam.serviceAccounts.actAs`
   through `roles/iam.serviceAccountUser` on each account; `iam:PassRole` on each role, to the one service that
   uses it) and nothing more.
4. **Scoped by name and tag.** GCP grants are on the resource itself where it exists (the bucket, the dataset, a
   service account, the subnet, and once the vendor has created them the Cloud Run service and jobs). Otherwise
   they are on the project with an IAM condition that **fails closed**: it names the resource type and the name
   (`resource.type == "…" && resource.name…`), so a resource it can't recognise gets nothing. GCP gives conditions
   resource attributes for GKE clusters, Secret Manager and Compute Engine, not for Cloud Run, so Cloud Run grants
   are never conditioned: they are made on the resource, or (the vendor's create) on the project and documented
   as project-wide. AWS grants name ARNs with the install's prefix, and
   every create must carry the install's tag (`memstore-install = <name>`) while every change or delete needs a
   resource that already carries it. `ec2:CreateTags` is allowed only as part of a create
   (`ec2:CreateAction`), so the vendor can't adopt one of the customer's resources by tagging it.
5. **A project (GCP) of AmpUp Memory's own.** See [Workload Identity and the dedicated project](#workload-identity-and-the-dedicated-project).
6. **No wildcard actions.** Every action is named. `Resource "*"` appears only where AWS defines no resource type
   for the action, listed in [Wildcards](#wildcards-and-resource-), plus one `Deny` on the bucket.

## Who creates the bucket and the dataset

The vendor's role can read the memory bucket's metadata (`storage.buckets.get`) and the dataset
(`bigquery.datasets.get`), and can't create, change or delete either. So with these templates the customer's
module creates them, and the install **adopts** them by name:

| Variant | Bucket | BigQuery dataset | Who creates them |
|---|---|---|---|
| `deploy/iam/gcp/byoc`, `byovpc` | `var.bucket` | `<name>_api` (`memstore_api` by default), only with `cloud_run = true` | the customer, applying the template |
| `deploy/gcp/terraform` with `existing_bucket`, `existing_dataset` | adopted (`data` sources) | adopted | nobody: the module checks and uses them |
| the same, with the template's accounts too (`existing_engine_sa`, `existing_ingest_sa`) | adopted | adopted | nobody, and the whole install runs on the vendor role: see [Installing as the vendor role alone](#installing-as-the-vendor-role-alone) |
| `deploy/gcp/terraform` without them (`bucket`) | created | created | whoever runs it (a project owner, not the vendor role) |

The template's `terraform_vars` output is those inputs, ready to pass:

```sh
terraform -chdir=deploy/iam/gcp/byoc output -raw terraform_vars
#   -var project=acme-memstore -var region=us-central1 -var name=ampup-memory -var existing_bucket=acme-memstore-memory
#   -var existing_dataset=memstore_api -var existing_engine_sa=memstore-engine@acme-memstore.iam.gserviceaccount.com
#   -var existing_ingest_sa=memstore-ingest@acme-memstore.iam.gserviceaccount.com
terraform -chdir=deploy/gcp/terraform apply -var image=<engine image> $(terraform -chdir=deploy/iam/gcp/byoc output -raw terraform_vars)
```

(Without `cloud_run = true` the template makes no dataset and its outputs are the bucket alone. Its `init_flags`
output carries the same inputs as flags for the hosted service's onboarding wizard, which is not part of this
repo.)

Routines the hosted onboarding wizard creates carry its marker as their description
(`Created by ampup-memory init; replaced on each apply.`); in an adopted dataset it replaces only routines carrying
it, and stops rather than replace a `search`, `episode` or `_whoami` without it. `deploy/gcp/terraform` manages
only the routines in its own state. The BigQuery connection is created by the install and named after `name`.

Adopting checks and never changes. The bucket must exist, be readable as whoever runs the module, and have uniform
bucket-level access with public access prevented; the dataset must exist, be readable, and be in `var.region` (the
remote functions call the connection there). Otherwise `deploy/gcp/terraform` refuses at plan (its `data` sources
fail to read, or their postconditions fail with the command that fixes it, for the resource's owner to run:
`gcloud storage buckets update …` when public, `bq mk --dataset …` when in another region). An account that can't
read them needs a read grant (`roles/storage.bucketViewer` on the bucket, `roles/bigquery.metadataViewer` on the
dataset). With the template's accounts the module makes no grant on the bucket; the
engine's object access there is the template's `<name>_engine_objects` grant. Re-running it changes nothing once
applied.

The module names its resources after `name` (default `ampup-memory`): `<name>-engine`, `<name>-load`,
`<name>-ingest`, the connection `<name>`, the names the template's Cloud Run grants are made on, so pass the
template's `name` (its `terraform_vars` does).

## Installing as the vendor role alone

With the template applied with `cloud_run = true`, `deploy/gcp/terraform` with `existing_engine_sa` and
`existing_ingest_sa` (in the template's `terraform_vars`) runs on `<name>-vendor@` and needs no project owner. The
template has already done what needs one, so the module doesn't redo it:

| | Without the template's accounts | With them (vendor role alone) |
|---|---|---|
| APIs | enabled (`google_project_service`) | none enabled (the template's) |
| Engine (and ingest) account | created | the template's `<name>-engine@` (and `<name>-ingest@`), read, never created |
| Project grants to the engine | made (`roles/biglake.editor`, `roles/bigquery.jobUser`, `roles/aiplatform.user`) | none made: the template's `<name>_engine_catalog`, `<name>_model_predict`, `<name>_bq_connection_use` and `roles/bigquery.jobUser` |
| Grants on the engine service, the jobs, and to the connection's account | made | the template's second apply (`cloud_run_deployed = true`, `bigquery_connection_service_account`), once the vendor has deployed; the module's `template_grants` check reads the service's policy (`run.services.getIamPolicy`) and warns until they are there, and its `template_next` output is the two `-var`s |
| The org (`ampup-memory org create … --direct`, run after the apply) | created by whoever installs | **the customer's**: it writes the memory catalog, which the vendor role can't read or write by design. The project's owner runs it as themselves |

The module runs scheduled ingest, so the template is applied with `scheduled_ingest = true` (the default; the
module refuses `existing_engine_sa` without `existing_ingest_sa`). In order:

1. The customer applies the template, then gives the vendor `vendor_service_account` and `terraform_vars`.
2. The vendor runs `terraform apply` in `deploy/gcp/terraform` as `<name>-vendor@`. It creates the catalog, the
   service, the jobs, the connection, the routines, the inbox, the scheduler and `_bin/ingest.sh`; the
   `template_grants` check warns that the engine refuses callers until step 3.
3. The customer applies the template again with `after_vendor_deploys` added (`-var cloud_run_deployed=true
   -var bigquery_connection_service_account=<the connection's account>`; the module's `template_next` output has
   it).
4. The customer creates the org (`ampup-memory org create <org> --admin <you> --direct`), as themselves.

## Revoking access

One variable cuts the vendor off without touching data or the running engine:

```sh
terraform apply -var vendor_access_enabled=false   # with the same other -var flags as the install
```

| | What the apply does | Takes effect |
|---|---|---|
| GCP | Removes `roles/iam.serviceAccountTokenCreator` for `var.vendor_principal` on the management service account, and disables that account. Its custom roles and bindings stay, attached to an account nobody can use. | New tokens at once. Access tokens already minted should be refused once the account is disabled (not confirmed here; see [Not verified](#not-verified)). Until then they last at most an hour. |
| AWS | Deletes the management role and its three policies. | New sessions at once. A session already open loses every permission, because they came from the deleted policies. |

Turn it back on with `vendor_access_enabled=true`. On GCP the account is re-enabled; on AWS a new role with the
same name is created (the vendor re-registers it, and an EKS access entry for the old role must be recreated).

Two more levers, for the cluster:

- **In-cluster access.** On AWS the vendor reaches the Kubernetes API through an EKS access entry for its role
  (EKS creates it for the cluster's creator). A session token for `sts:GetCallerIdentity` keeps working until it
  expires (at most 1 hour, `max_session_duration = 3600`), so delete the entry too:
  `aws eks delete-access-entry --cluster-name <name> --principal-arn <vendor role ARN>`. On GKE the vendor's
  Kubernetes access comes only from the IAM permissions above, so the apply removes it.
- **Upgrades.** The deployment agent runs inside the customer's cluster and keeps applying what the control plane
  signs (within the [ValidatingAdmissionPolicy](../deploy/agent/README.md#what-the-agent-may-change)). To stop
  upgrades and actions as well: `kubectl -n memstore scale deployment/memstore-agent --replicas=0`. The engine
  keeps serving. Its telemetry keeps flowing out until the deployment credential is revoked in the control
  plane or the Secret is removed.

Nothing in the revoke path deletes the bucket, the dataset, the Glue databases, a secret or a runtime
identity. `terraform destroy` of the whole module would try to delete the bucket and fail while it holds data
(`force_destroy = false`); don't use destroy to revoke.

## GCP

All roles are custom roles in the customer's project named `<name>_…` (dashes become underscores), unless a
predefined role is named. Every condition below is an IAM condition on the binding.

### Vendor management service account `<name>-vendor@<project>.iam.gserviceaccount.com`

Impersonated by `var.vendor_principal` (`roles/iam.serviceAccountTokenCreator` on this account only).

**`<name>_vendor_base`**, project, no condition.

| Permissions | Scope | Why | If revoked |
|---|---|---|---|
| `resourcemanager.projects.get` | project | Terraform reads the project (number, for service agents) | Installs and upgrades fail at plan |
| `compute.globalOperations.get`, `compute.regionOperations.get`, `compute.zoneOperations.get` | project | Poll the long-running operations its creates return (network, subnet, router, GKE node pools) | Creates time out unconfirmed |
| `container.clusters.list` | project | Terraform and `gcloud` list clusters (read-only). Create, update, delete and credentials are in `<name>_vendor_cluster`, below | Plans that list clusters fail |
| `container.operations.get`, `.list` | project | Wait for cluster operations | Cluster changes time out unconfirmed |
| `container.namespaces.create`, `.get` | every namespace (GKE IAM has no per-namespace scope) | The `ampup-memory` namespace | First install fails |
| `container.serviceAccounts.create`, `.delete`, `.get`, `.update` | every namespace | The engine's and the agent's Kubernetes service accounts (`deploy/agent/k8s/agent.yaml`) | Install fails |
| `container.roles.create`, `.delete`, `.get`, `.update`, `.bind`, `.escalate` | every namespace | The agent's Role (get/patch on the engine's Deployment only). Kubernetes refuses to create a Role granting rights the creator lacks unless it holds `escalate`; `bind` lets it bind that Role. `.escalate` is a TESTING-stage permission in custom roles | Agent can't be installed |
| `container.roleBindings.create`, `.delete`, `.get`, `.update` | every namespace | Bind the agent's Role to its service account | Agent can't act |
| `container.configMaps.create`, `.delete`, `.get`, `.update` | every namespace | Engine configuration; the agent's cosign public key | Install fails |
| `container.secrets.create`, `.delete`, `.update` (no `get`, no `list`) | every namespace | Write the deployment credential Secret. The vendor can't read any Secret back | Credential can't be written or rotated by the vendor |
| `container.persistentVolumeClaims.create`, `.delete`, `.get`, `.update` | every namespace | The agent's state volume | Agent can't keep state |
| `container.deployments.create`, `.delete`, `.get`, `.getStatus`, `.update` | every namespace | The engine and agent Deployments; read rollout status | No installs or vendor-driven upgrades |
| `container.services.create`, `.delete`, `.get`, `.update` | every namespace | The engine's Service | Engine unreachable inside the cluster |
| `container.jobs.create`, `.delete`, `.get`, `.getStatus`, `.update` | every namespace | Load and index jobs | No vendor-run loads |
| `container.pods.get`, `.list` | every namespace | Watch rollouts and diagnose. No exec, attach, port-forward, proxy or logs | Vendor can't see pod state |

With `cloud_run = true` it also holds:

| Permissions | Scope | Why | If revoked |
|---|---|---|---|
| `run.operations.get` | project | Wait for Cloud Run deploys | Deploys unconfirmed |
| `cloudscheduler.jobs.create`, `.delete`, `.enable`, `.get`, `.pause`, `.update` (only with `scheduled_ingest`, the default) | project | The scheduled-ingest trigger (`deploy/gcp/terraform`) | No scheduled ingest changes |
| `bigquery.connections.create`, `.delete`, `.get`, `.update` | project | The CLOUD_RESOURCE connection the remote functions call through | No BigQuery surface |
| `bigquery.connections.delegate` | project | Required to create a remote function on the connection | Remote functions can't be created |
| `bigquery.jobs.create` | project | Run the DDL that creates the routines and the inbox (`deploy/gcp/sql`) | SQL surface can't be (re)created |
| `biglake.catalogs.create`, `.get` | project | The BigLake Iceberg catalog over the memory bucket. No catalog update or delete | No catalog: the engine can't start on a new install |
| `serviceusage.services.list` | project | Read which APIs are enabled, so the hosted onboarding wizard (not part of this repo) checks the template enabled them rather than enabling any (no `serviceusage.services.enable`). `deploy/gcp/terraform` doesn't read it | the hosted wizard stops at its first row |
| `resourcemanager.projects.getIamPolicy` | project | Read the project's IAM policy, so the hosted onboarding wizard (not part of this repo) checks the template's grants to the engine and the connection's account rather than making them. Read-only: the vendor still can't change a policy | the hosted wizard stops at the service account |

**`<name>_vendor_cluster`** (GKE), project, condition:
`resource.type == "container.googleapis.com/Cluster" && resource.name.extract("/clusters/{c}") == "<name>"`.

| Permissions | Why | If revoked |
|---|---|---|
| `container.clusters.create`, `.delete`, `.get`, `.update` | Create and upgrade the cluster named `<name>` and its node pools; delete it on uninstall. No other cluster: a second cluster in the project could run pods as `memstore/memstore`, which Workload Identity would treat as the engine | No cluster: nothing runs |
| `container.clusters.getCredentials` | A kubeconfig for that cluster, to apply the workloads | No deploys or upgrades from the vendor |

**`<name>_vendor_run`** (Cloud Run only), project, **no condition**. Cloud Run gives IAM conditions no resource
attributes, and a create can only be granted on the project, so this covers every Cloud Run service and job in
the project. That is one reason the project must be AmpUp Memory's own. (The earlier draft narrowed it with
`resource.type != "run.googleapis.com/Service" || …`, which matched everything and narrowed nothing.)

| Permissions | Why | If revoked |
|---|---|---|
| `run.services.create`, `.delete`, `.get`, `.update` | The engine service | No installs or upgrades on Cloud Run |
| `run.jobs.create`, `.delete`, `.get`, `.update` | The load and ingest jobs. No `run`, `runWithOverrides` or `setIamPolicy`: the vendor can't execute a job or open a service to callers | Jobs can't be changed |
| `run.services.getIamPolicy` | Read the engine service's policy: it must not be public, and the template's grants on it must be there (`deploy/gcp/terraform`'s `template_grants` check, and the hosted onboarding wizard). Read-only | the `template_grants` check can't confirm the grants (the hosted wizard stops at the Cloud Run service) |

**`<name>_vendor_secrets`**, project, condition: the resource is a secret or a secret version, and its name starts
with `projects/<project number>/secrets/<secret_prefix>`.

| Permissions | Why | If revoked |
|---|---|---|
| `secretmanager.secrets.create`, `.get`, `secretmanager.versions.add`, `.get`, `.list` | Write the deployment credential (and rotate it by adding a version). No `versions.access`: the vendor never reads a value | Vendor can't install or rotate the credential |

**`<name>_vendor_bucket`**, on the memory bucket.

| Permissions | Why | If revoked |
|---|---|---|
| `storage.buckets.get` | Point the catalog and jobs at the bucket. No object access, no bucket update or delete | Catalog and job creation fail |

**`<name>_vendor_bin`** (Cloud Run with `scheduled_ingest`), on the memory bucket, condition: object name starts with `_bin/`.

| Permissions | Why | If revoked |
|---|---|---|
| `storage.objects.create`, `.delete`, `.get` | Upload `_bin/ingest.sh`, the script the ingest job runs (and replace it on upgrade) | Ingest script can't be updated |

**`<name>_vendor_bq_ddl`** (Cloud Run only), on the `<name>_api` dataset.

| Permissions | Why | If revoked |
|---|---|---|
| `bigquery.datasets.get` | Read the dataset the routines go in | DDL fails |
| `bigquery.routines.create`, `.delete`, `.get`, `.update` | `search`, `episode`, `_whoami`, `remember`, `ask` | SQL surface can't be (re)created |
| `bigquery.tables.create`, `.get`, `.update` | The `inbox` table's DDL. No `getData`/`updateData`: the vendor reads no rows | Inbox can't be created |

**`roles/iam.serviceAccountUser`** on each runtime service account the vendor attaches to a workload: the engine
(Cloud Run service and jobs), ingest (Cloud Run job and the Scheduler's OAuth identity; only with `scheduled_ingest`)
and nodes (GKE node pool).
It holds `iam.serviceAccounts.actAs`, `.get`, `.list` on that account only. Not on the agent's account: Workload
Identity binds it, not the vendor. If revoked, the vendor can't create or update those workloads.

**`<name>_vendor_network`** (BYOC only), project, condition: the resource is a network, subnetwork or router whose
own name starts with `<name>` (each type checked with its own name).

| Permissions | Why | If revoked |
|---|---|---|
| `compute.networks.create`, `.delete`, `.get`, `.use` | The install's VPC | No network for a new cluster |
| `compute.subnetworks.create`, `.delete`, `.get`, `.update`, `.use`, `.setPrivateIpGoogleAccess` | Its subnet with pod and service ranges; Private Google Access for private nodes | Same |
| `compute.routers.create`, `.delete`, `.get`, `.update` | A Cloud Router with Cloud NAT, so private nodes reach the control plane and the registry | Nodes have no egress |

No firewall, route, peering or address permissions: GKE's own service agent manages its firewall rules.

**BYOVPC instead:** `roles/compute.networkUser` on the customer's one subnetwork (`var.subnetwork` in
`var.network_project`), so GKE nodes and Cloud Run egress can be placed in it. The vendor holds no permission on
any other subnet or on the network itself. For a Shared VPC (the subnet in another project) the module also
grants Google's service agents what Shared VPC requires: `roles/compute.networkUser` on the subnet to
`<number>@cloudservices`, the GKE service agent and (Cloud Run) the serverless service agent, and
`roles/container.hostServiceAgentUser` on the host project to the GKE service agent.

### Engine `<name>-engine@`

| Permissions | Scope | Why | If revoked |
|---|---|---|---|
| `<name>_engine_objects`: `storage.objects.create`, `.delete`, `.get`, `.list` | the memory bucket | Iceberg data and metadata, episode bodies, the Lance search index under `_index/` | The engine can't read or write memory |
| `<name>_engine_catalog`: `biglake.catalogs.get`; `biglake.namespaces.create`, `.delete`, `.get`, `.list`, `.update`; `biglake.tables.create`, `.delete`, `.get`, `.list`, `.lock`, `.update` | project (BigLake has no documented name conditions) | The Iceberg REST catalog (`src/platform.rs`, `gcp_props`): commit snapshots, create scopes' namespaces and tables, drop them on `workspace delete`. `roles/biglake.editor` minus catalog create/update/delete and IAM reads | No commits, no reads of table metadata |
| `<name>_model_predict`: `aiplatform.endpoints.predict` | project (publisher models have no per-model IAM) | `ask` answers through Gemini `generateContent`; extraction with `AMPUP_MEMORY_EXTRACT=vertex` | No Gemini answers or extraction |
| `roles/secretmanager.secretAccessor` | condition: secret id starts with `<secret_prefix>` | Model API keys (`ANTHROPIC_API_KEY`) and the deployment credential, mounted from Secret Manager (`deploy/gcp/trial`) | Engine can't start with those secrets |
| Workload Identity: `roles/iam.workloadIdentityUser` for `<project>.svc.id.goog[memstore/memstore]` | this account; matches that namespace and service account in **every** GKE cluster of the project (see below) | The engine's pods act as this account on GKE (set with `gke_workload_identity_ready = true` once the cluster exists) | Engine pods have no Google identity |

With `cloud_run = true` also:

| Permissions | Scope | Why | If revoked |
|---|---|---|---|
| `<name>_engine_run_self`: `run.services.get`, `run.services.getIamPolicy` | the `<name>-engine` service only (bound on it once `cloud_run_deployed = true`) | The engine trusts Cloud Run-verified caller tokens only after reading its own service and its IAM policy (`src/platform.rs`) | The engine refuses every caller |
| `roles/bigquery.jobUser` | project (jobs are project-level) | `remember_memory` writes the inbox and `ask_memory` calls Gemini through BigQuery | Those MCP tools fail |
| `roles/bigquery.dataEditor` | the `<name>_api` dataset | Write and read the inbox | Same |
| `<name>_bq_connection_use`: `bigquery.connections.get`, `.use` | project | Call `_whoami` through the connection at start | The engine can't learn BigQuery's caller id |

The engine also reads group memberships from the Cloud Identity API (the template enables `cloudidentity` with
`cloud_run = true`, and `cloudresourcemanager`, which the project-policy reads above go through). That is not an IAM grant: a Workspace
admin gives the engine's account the **Groups Reader** admin role. Without it, callers get no groups (fewer
rights, never more).

### Ingest `<name>-ingest@` (Cloud Run with `scheduled_ingest`, the default)

Set `scheduled_ingest = false` (for an install that runs no scheduled ingest; `deploy/gcp/terraform` always runs it) and none of this exists.

| Permissions | Scope | Why | If revoked |
|---|---|---|---|
| `<name>_engine_objects` | the memory bucket | Stage a batch under `_corpora/` and remove it after the load (`deploy/gcp/ingest.sh`) | No scheduled ingest |
| `roles/bigquery.jobUser` | project | The extraction queries | Same |
| `roles/bigquery.dataEditor` | the `<name>_api` dataset | Read and mark the inbox | Same |
| `<name>_bq_connection_use` | project | `AI.GENERATE` through the connection | Same |
| `<name>_ingest_run_jobs`: `run.jobs.get`, `.run`, `.runWithOverrides`, `run.executions.get` | the `<name>-load` job only (bound on it once `cloud_run_deployed = true`) | Run the load job with per-batch arguments and wait for its execution | Same |
| `roles/run.invoker` | the `<name>-ingest` job only (same) | Cloud Scheduler starts the ingest job with this account's token | Same |
| `<name>_run_operations`: `run.operations.get` | project (Cloud Run operations can't be bound per resource) | Read the status of the long-running call `gcloud run jobs execute --wait` made. Read-only | `--wait` fails |

### BigQuery connection's service account (Cloud Run only)

Exists only after the vendor creates the connection. Pass it as `bigquery_connection_service_account`, with
`cloud_run_deployed = true`, and apply again.

| Permissions | Scope | Why | If revoked |
|---|---|---|---|
| `<name>_model_predict` | project | `AI.GENERATE` in `ask()` and ingest | No Gemini in SQL |
| `roles/run.invoker` | the `<name>-engine` service only, bound on it | The `search`, `episode` and `_whoami` remote functions call the engine | The SQL surface fails |

No runtime identity holds a Cloud Run role on the project; only `run.operations.get`. Until
`cloud_run_deployed = true` none of the Cloud Run grants exist, and the engine refuses callers (it can't read its
own service), which fails closed.

### Workload Identity and the dedicated project

`roles/iam.workloadIdentityUser` is granted to `serviceAccount:<project>.svc.id.goog[memstore/memstore]` (and
`[memstore/memstore-agent]`). That member names the project's one workload identity pool, not a cluster: a pod
running as `memstore/memstore` in **any** GKE cluster of the project gets the engine's Google identity, and with it
the memory bucket. GKE's own documentation says IAM "doesn't distinguish between the clusters" in a pool and that
you "must be able to trust every cluster in a specific workload identity pool".

GKE Workload Identity Federation has no principal that names one service account in one cluster. Its forms are
`subject/ns/<ns>/sa/<ksa>` (every cluster in the pool, the same as above), `kubernetes.serviceaccount.uid/<uid>`
(not documented for impersonating a Google service account, and it would break silently when the Kubernetes
service account is recreated), and `namespace/<ns>` or `kubernetes.cluster/<url>` (wider: every pod in a
namespace or a cluster). So the modules don't try to narrow the binding. Instead:

- **The project must be AmpUp Memory's alone.** No one else may create GKE clusters (or Cloud Run services, which
  the vendor's role covers project-wide) in it. The GCP modules refuse to plan until you set
  `dedicated_project = true` to say so.
- **The vendor can create only one cluster**, the one named `<name>` (`<name>_vendor_cluster`, above).
- Anyone else with `container.clusters.create` in the project could still become the engine. Grant it to no one
  else, and watch Cloud Audit Logs for `google.container.v1.ClusterManager.CreateCluster` in the project.

### Agent `<name>-agent@` (GKE)

**No role in the customer's project.** The agent only calls out (`GET /v1/desired`, `/v1/actions` on the control
plane) and reads image signatures. Its Kubernetes rights are its own Role: `get` and `patch` on the engine's one
Deployment, further limited by the ValidatingAdmissionPolicy in [`deploy/agent/k8s/agent.yaml`](../deploy/agent/k8s/agent.yaml).
That policy is cluster-scoped and GKE IAM has no permission for admission policies, so a cluster admin of the
customer applies it, and the vendor's identity can't change or remove it.

Workload Identity binds it to `memstore/memstore-agent` (with `gke_workload_identity_ready = true`).

### Nodes `<name>-nodes@` (GKE)

| Permissions | Scope | Why | If revoked |
|---|---|---|---|
| `roles/container.defaultNodeServiceAccount` | project | Node logging, monitoring and autoscaler metrics. Replaces the Compute Engine default account, which holds Editor | Nodes stop reporting; GKE warns |

### Granted on the vendor's side

The engine image and its cosign signatures are in the vendor's Artifact Registry. The vendor grants
`roles/artifactregistry.reader` on those repositories to the customer's `agent_service_account` (to read
signatures, `deploy/agent/README.md`) and `nodes_service_account` (to pull the image). Nothing in the customer's
project is needed for this.

## AWS

Names are `<name>-…`; the install's tag is `memstore-install = <name>` (`var.tag_key`). ARNs below drop the
`arn:aws:<service>:<region>:<account>:` prefix.

### Vendor management role `<name>-vendor`

Trust: `sts:AssumeRole` by `var.vendor_principal_arn` (one role in the vendor's account), only with
`sts:ExternalId` equal to the id the vendor issued this customer. Sessions last at most 1 hour.

**Policy `<name>-vendor-network`, BYOC.**

| Actions | Resource | Condition | Why | If revoked |
|---|---|---|---|---|
| `ec2:DescribeAccountAttributes`, `DescribeAddresses`, `DescribeAddressesAttribute`, `DescribeAvailabilityZones`, `DescribeInternetGateways`, `DescribeNatGateways`, `DescribeNetworkAcls`, `DescribeNetworkInterfaces`, `DescribeRouteTables`, `DescribeSecurityGroupRules`, `DescribeSecurityGroups`, `DescribeSubnets`, `DescribeTags`, `DescribeVpcAttribute`, `DescribeVpcs` | `*` | none | Terraform reads what it created. EC2 Describe calls have no resource type (`DescribeVpcAttribute` has one, but is read-only metadata) | Plans fail |
| `ec2:CreateVpc` | `vpc/*` | request carries the install's tag | The install's VPC | No network |
| `ec2:CreateSubnet` | `subnet/*`; `vpc/*` | tag in the request; the VPC carries the tag | Private subnets for nodes, public ones for NAT | Same |
| `ec2:CreateInternetGateway` | `internet-gateway/*` | tag in the request | Egress for the NAT gateway | Same |
| `ec2:AllocateAddress` | `elastic-ip/*` | tag in the request | The NAT gateway's address | Same |
| `ec2:CreateNatGateway` | `natgateway/*`; `subnet/*`, `elastic-ip/*`, `vpc/*` | tag in the request; the subnet, address and VPC carry it | Egress for private nodes (control plane, registry) | Same |
| `ec2:CreateRouteTable` | `route-table/*`; `vpc/*` | tag in the request; the VPC carries it | Routes for the subnets | Same |
| `ec2:CreateSecurityGroup` | `security-group/*`; `vpc/*` | tag in the request; the VPC carries it | Node and endpoint security groups | Same |
| `ec2:AssociateRouteTable`, `AttachInternetGateway`, `AuthorizeSecurityGroupEgress`, `AuthorizeSecurityGroupIngress`, `CreateRoute`, `DeleteInternetGateway`, `DeleteNatGateway`, `DeleteRoute`, `DeleteRouteTable`, `DeleteSecurityGroup`, `DeleteSubnet`, `DeleteVpc`, `DetachInternetGateway`, `DisassociateRouteTable`, `ModifySubnetAttribute`, `ModifyVpcAttribute`, `ReleaseAddress`, `RevokeSecurityGroupEgress`, `RevokeSecurityGroupIngress` | `vpc/*`, `subnet/*`, `internet-gateway/*`, `elastic-ip/*`, `natgateway/*`, `route-table/*`, `security-group/*` | the resource carries the install's tag | Wire up, change and remove only what it created | Network can't be changed or torn down |
| `ec2:AuthorizeSecurityGroupEgress`, `AuthorizeSecurityGroupIngress` | `security-group-rule/*` | none (the group itself must be tagged, above) | A rule is a new resource with no tag of its own | Rules can't be added |
| `ec2:CreateTags` | the seven types above | `ec2:CreateAction` is one of the creates above | Tag on create, so the creates can carry the tag. Never an existing resource | Creates fail |

**Policy `<name>-vendor-network`, BYOVPC.** The Describe statement above, and only:

| Actions | Resource | Condition | Why | If revoked |
|---|---|---|---|---|
| `ec2:CreateSecurityGroup` | `security-group/*`; `vpc/<var.vpc_id>` | tag in the request | Security groups for nodes and endpoints, in the customer's VPC only | Install fails |
| `ec2:AuthorizeSecurityGroupEgress`, `AuthorizeSecurityGroupIngress`, `DeleteSecurityGroup`, `RevokeSecurityGroupEgress`, `RevokeSecurityGroupIngress` | `security-group/*` | the group carries the tag | Manage only its own groups | Groups can't be changed |
| `ec2:AuthorizeSecurityGroupEgress`, `AuthorizeSecurityGroupIngress` | `security-group-rule/*` | none | As above | As above |
| `ec2:CreateTags` | `security-group/*` | `ec2:CreateAction = CreateSecurityGroup` | Tag on create | Creates fail |

No VPC, subnet, route, gateway or address action. `eks:CreateCluster` and `eks:CreateNodegroup` take subnet ids
that IAM can't restrict (EKS has no subnet condition key), so the subnets are a contract, not an enforcement:
`subnet_ids` is an output for the vendor, and a cluster in another subnet would show in CloudTrail
(`CreateCluster` request parameters).

**Policy `<name>-vendor-eks`.**

| Actions | Resource | Condition | Why | If revoked |
|---|---|---|---|---|
| `eks:CreateCluster` | `*` (the action has no resource type) | request carries the install's tag; with `allow_public_cluster_endpoint = false`, `eks:endpointPublicAccess = false` | The cluster, named `<name>` | No cluster |
| `eks:CreateAddon`, `CreateNodegroup`, `CreatePodIdentityAssociation`, `DeleteCluster`, `DescribeCluster`, `DescribeUpdate`, `ListAddons`, `ListNodegroups`, `ListPodIdentityAssociations`, `TagResource`, `UpdateClusterVersion` | `cluster/<name>` | the cluster carries the tag | Node group, the Pod Identity add-on, associations for the engine and agent; Kubernetes upgrades; uninstall | No node group, no pod credentials, no upgrades |
| `eks:UpdateClusterConfig` | `cluster/<name>` | tagged; and `eks:endpointPublicAccess = false` when public endpoints are not allowed | Logging, endpoint access | Config can't be changed |
| `eks:DeleteAddon`, `DeleteNodegroup`, `DeletePodIdentityAssociation`, `DescribeAddon`, `DescribeNodegroup`, `DescribePodIdentityAssociation`, `DescribeUpdate`, `UpdateAddon`, `UpdateNodegroupConfig`, `UpdateNodegroupVersion` | `addon/<name>/*`, `nodegroup/<name>/*`, `podidentityassociation/<name>/*` | none (only the install's cluster) | Day-2 changes to the cluster's parts | Same |
| `eks:DescribeAddonVersions` | `*` (no resource type) | none | Pick add-on versions | Add-on upgrades fail |
| `iam:PassRole` | `role/<name>-cluster` | `iam:PassedToService = eks.amazonaws.com` | The control plane's role | No cluster |
| `iam:PassRole` | `role/<name>-nodes` | `iam:PassedToService` is `eks.amazonaws.com` or `ec2.amazonaws.com` | The node group's role | No nodes |
| `iam:PassRole` | `role/<name>-engine`, `role/<name>-agent` | `iam:PassedToService = pods.eks.amazonaws.com` | Pod Identity associations | Pods have no AWS credentials |
| `iam:GetRole` | the four roles above | none | Terraform reads the roles it passes | Plans fail |
| `iam:CreateServiceLinkedRole` | `role/aws-service-role/*` | `iam:AWSServiceName` is `eks.amazonaws.com` or `eks-nodegroup.amazonaws.com` | EKS's service-linked roles, the first time in an account | First cluster in an account fails |
| `kms:CreateGrant`, `kms:DescribeKey` (only with `cluster_kms_key_arn`) | that key | `kms:GrantIsForAWSResource = true` | EKS envelope encryption of Kubernetes Secrets with the customer's key | Cluster can't use the key |

No `eks:CreateAccessEntry` or `eks:AssociateAccessPolicy`: the vendor can't grant anyone Kubernetes access. Its
own comes from EKS's creator entry (see [Revoking access](#revoking-access)).

**Policy `<name>-vendor-ops`.**

| Actions | Resource | Condition | Why | If revoked |
|---|---|---|---|---|
| `secretsmanager:CreateSecret`, `TagResource` | `secret:<secret_prefix>*` | request carries the install's tag | The deployment credential | Credential can't be stored |
| `secretsmanager:DescribeSecret`, `PutSecretValue` | `secret:<secret_prefix>*` | the secret carries the tag | Rotate the credential. No `GetSecretValue`, no delete | Rotation fails |
| `logs:CreateLogGroup`, `ListTagsForResource`, `PutRetentionPolicy`, `TagResource` | `log-group:/aws/eks/<name>/*` | none | The control plane's log group, with retention | No control-plane logs |
| `logs:DescribeLogGroups` | `*` (no resource type) | none | Terraform reads the log group | Plans fail |

### Engine role `<name>-engine`

Trust: EKS Pod Identity (`pods.eks.amazonaws.com`) from this account, and `sts:TagSession` only when the session
tags say cluster `<name>`, namespace `ampup-memory`, service account `ampup-memory`. Another pod, or another cluster,
can't assume it even if an association names it.

| Actions | Resource | Condition | Why | If revoked |
|---|---|---|---|---|
| `s3:ListBucket` | the memory bucket | none | List table and index files | The engine can't read memory |
| `s3:AbortMultipartUpload`, `DeleteObject`, `GetObject`, `PutObject` | the memory bucket's objects | none | Iceberg data and metadata, episode bodies, the Lance index | Same |
| `glue:CreateDatabase`, `DeleteDatabase`, `GetDatabase`, `GetDatabases`, `UpdateDatabase`, `CreateTable`, `DeleteTable`, `GetTable`, `GetTables`, `UpdateTable` | the account's catalog, `database/<glue_database_prefix>*`, `table/<glue_database_prefix>*/*` | none | The Iceberg catalog on Glue (planned AWS adapter); `UpdateTable` is the commit | No commits |
| `secretsmanager:DescribeSecret`, `GetSecretValue` | `secret:<secret_prefix>*` | none | The deployment credential and model API keys | Engine can't start with them |
| `bedrock:InvokeModel`, `InvokeModelWithResponseStream` | `bedrock_model_arns` (none by default) | none | Extraction and answers on Bedrock with the role's own credentials (planned AWS adapter) | No Bedrock calls |
| `kms:Decrypt`, `GenerateDataKey` (only with `kms_key_arn`) | that key | `kms:ViaService = s3.<region>.amazonaws.com` | SSE-KMS on the bucket | The engine can't read or write memory |

The bucket's policy also denies every S3 action over plain HTTP (`Deny`, `s3:*`, `aws:SecureTransport = false`).

### Agent role `<name>-agent`

Same trust, for the service account `memstore-agent`. With `image_repository_arns` (the vendor's ECR
repositories), it may pull image signatures:

| Actions | Resource | Why | If revoked |
|---|---|---|---|
| `ecr:GetAuthorizationToken` | `*` (no resource type) | The registry login token | The agent can't verify images, so it applies no upgrade |
| `ecr:BatchCheckLayerAvailability`, `BatchGetImage`, `GetDownloadUrlForLayer` | those repositories | Read the `.sig` manifests and layers | Same |

Without ECR it holds nothing. Its Kubernetes rights are its Role and the ValidatingAdmissionPolicy, as on GKE.

### Cluster and node roles

| Role | Policies (AWS managed) | Why |
|---|---|---|
| `<name>-cluster` (trusts `eks.amazonaws.com`) | `AmazonEKSClusterPolicy` | The EKS control plane's documented minimum |
| `<name>-nodes` (trusts `ec2.amazonaws.com`) | `AmazonEKSWorkerNodePolicy`, `AmazonEKS_CNI_Policy`, `AmazonEC2ContainerRegistryPullOnly` | Join the cluster, assign pod IPs, pull images. The registry policy is account-wide by design; cross-account pulls also need the vendor's repository policy |

## Wildcards and `Resource "*"`

No action is a wildcard. `Resource "*"` is used only for:

- the EC2 `Describe*` calls listed above (read-only; no resource type);
- `eks:CreateCluster` (no resource type; tag condition), `eks:DescribeAddonVersions`, `logs:DescribeLogGroups`,
  `ecr:GetAuthorizationToken` (no resource type);
- the bucket policy's `Deny` of `s3:*` without TLS, which only takes rights away.

The offline tests in `deploy/iam/*/*/tests` fail if a vendor policy gains a wildcard action, a `Resource "*"` on
any other action, a data-access or IAM-admin action, or untagged `ec2:CreateTags`; on GCP, if a vendor role gains
`setIamPolicy`, object access outside `_bin/`, a secret read, table data, or (BYOVPC) a network permission; if any
condition has a negated (fail-open) clause; if the vendor gains a `serviceusage` permission other than listing; if a Cloud Run grant is made anywhere but the engine service and the
two jobs; if cluster lifecycle permissions leave the cluster-name condition; or if a plan without
`dedicated_project = true` succeeds.

## Not verified

The modules pass `terraform validate` and offline plan tests, and every AWS action and GCP permission name was
checked against AWS's service reference and `gcloud iam list-testable-permissions`
(`deploy/iam/check_permissions.py`). None of it has been applied: an install in a scratch project and a scratch
account, driven by the vendor role, is what will show any permission that is missing. The likeliest gaps:

- **Fail-closed conditions on creates.** GKE, Secret Manager and Compute Engine support resource attributes, but
  Google doesn't document whether a *create* (`container.clusters.create`, `secretmanager.secrets.create`,
  `compute.networks.create`) is checked against the new resource's name or against the project. If it is the
  project, the condition refuses the create (closed, not open): the fix is then a create-only role for the
  vendor on the project, documented here as project-wide, or the customer creating that one resource.
- **`run.executions.get` granted on the load job** is assumed to cover that job's executions.
- **BigLake.** Whether the Iceberg REST catalog needs a permission outside `<name>_engine_catalog` (it is
  `roles/biglake.editor` minus catalog admin, which the existing modules use).
- **GKE in BYOVPC** may also want `compute.networks.use` on the customer's network when creating the cluster.
- **Disabling a GCP service account** to cut off access tokens already minted: expected, not confirmed or timed here.
- **AWS Pod Identity trust** with session-tag conditions on `sts:TagSession`; **EKS tag-on-create** for node
  groups and add-ons may also want `eks:TagResource` on those resources.
- **The AWS engine adapter isn't built.** The engine role is what that adapter will need; its Glue
  namespaces must use `glue_database_prefix`, and today's Bedrock path authenticates with an API key
  (`AWS_BEARER_TOKEN_BEDROCK`), not the role. The agent can't yet authenticate to ECR (`gcp-metadata`,
  `anonymous` and `none` only).
- **Not covered:** a load balancer in front of the engine (GCP Ingress, an AWS ALB) and ECS
  instead of EKS.
- **`deploy/gcp/terraform` as the vendor role** ([above](#installing-as-the-vendor-role-alone))
  is tested with offline plans (`terraform test`); not yet run in a project. In particular: whether creating a new
  Cloud Run service reads or sets its IAM policy anyway; whether creating the BigQuery connection needs more than
  `bigquery.connections.create`. (The engine, started before the template's second apply, refuses
  callers until its periodic refresh can read its service's policy, then accepts them.)
