# AmpUp Memory in your GCP project

For the admin who owns the project. AmpUp Memory runs inside your project, on identities your Terraform creates, with
its memory in a bucket you own. The vendor never holds a key to your project, can't read the memory, and can be
revoked with one variable while the engine keeps running. Plan about 40 minutes the first time.

This directory is written for you. It names nothing of the vendor's own infrastructure beyond the two hosts your
project will reach (the control plane and the image registry), and everything it asks you to run is printed
before it runs.

## The five steps

| | You run | What it does | Writes to your project |
|---|---|---|---|
| 1 | `ampup-memory preflight gcp --project …` | checks the project and names every blocker with its fix | nothing |
| 2 | `terraform apply` in `deploy/iam/gcp/byoc` (or `byovpc`) | creates the vendor's management identity, the runtime identities, the bucket | yes, as you |
| 3 | `terraform apply` in `deploy/gcp/terraform` | installs the engine, as the vendor identity from step 2 | yes, as the vendor role |
| 4 | `ampup-memory preflight gcp --installed --cosign-key …` | checks the running engine: Ready, private, healthy, its image signed by the vendor's key, the data stores private, the enrollment live | nothing |
| 5 | policy, support, offboarding | see [operations](operations.md) | as you choose |

When the vendor runs AmpUp Memory for you as a managed service, it pairs and installs with its own onboarding wizard,
which is part of the hosted service and not in this repo; step 3 is then the vendor's.

## What you need before step 1

| Need | Exactly | Why |
|---|---|---|
| A project for AmpUp Memory alone | the template's `dedicated_project = true` confirms nobody else creates GKE clusters or Cloud Run services in it | Workload Identity trusts the engine's identity in every cluster of the project |
| Billing | an active billing account linked to the project | BigQuery, Cloud Run and GCS refuse a project without one |
| Who applies the IAM template (step 2) | a project Owner, or Editor plus Project IAM Admin, Role Administrator and Service Account Admin; once | it creates custom roles, service accounts and grants |
| Who runs the preflight (step 1) | anyone with Viewer on the project | it only reads |
| APIs | the template enables them (`iam`, `iamcredentials`, `storage`, `biglake`, `aiplatform`, `secretmanager`, `compute`, plus `run`, `bigquery`, `bigqueryconnection`, `cloudidentity`, `cloudscheduler` for Cloud Run or `container` for GKE); the preflight lists any that are off | a describe on a disabled API fails |
| Quota in the region | Cloud Run: 7 vCPU of CPU allocation. GKE: 12 N2 vCPUs, 300 GB persistent SSD, 2 in-use addresses | the preflight reports limit, usage and gap by quota name |
| Org policies | domain-restricted sharing must allow the vendor's customer id (the preflight takes it as `--vendor-customer-id`); service-account key creation may stay disabled (AmpUp Memory makes none); external IPs may stay forbidden (the template makes Cloud NAT) | the commonest reasons an install fails in an enterprise project |
| Network | outbound 443 to the control plane and the image registry; BYOVPC: one subnet with Private Google Access, and Cloud NAT or a proxy | the engine's agent pulls; nothing of the vendor's connects in |
| A model key | an extraction model key in Secret Manager under the `memstore-` prefix, or BigQuery `AI.GENERATE` (no key) | the engine reads it; it is never in Terraform state |
| Tools | `gcloud`, `terraform` 1.5 or later, the `ampup-memory` CLI | Cloud Shell has the first two; the CLI's installer prints its checksum |
| From the vendor | the principal to grant, the image repository, the cosign public key with its fingerprint, the control plane URL, the vendor's GCP customer id | each appears on the page the vendor gives you with your org |

## Step 1: preflight

```sh
ampup-memory preflight gcp --project <project> --region us-central1 \
  --variant byoc --runtime cloud-run \
  --vendor-customer-id <from the vendor> \
  --control-plane <from the vendor> --image-registry us-docker.pkg.dev
```

It prints each `gcloud` command as it runs it, then one row per check: PASS, WARN, FAIL or SKIP. A FAIL always
carries the command that fixes it. Fix the FAIL rows and run it again until none is left; WARN rows say what to
expect and don't block. `--json` writes the same rows as one JSON object, for a ticket or a CI step; `--explain`
prints what every check reads and when it fails ([preflight.md](preflight.md) is that output).

Exit codes: 0 all pass, 1 warnings only, 2 any failure, 3 the preflight itself couldn't run (no `gcloud`, nobody
signed in, the project unreadable).

## Step 4: verify

```sh
ampup-memory preflight gcp --project <project> --installed --bucket <bucket> \
  --cosign-key cosign.pub --signature-repository <from the vendor>
```

The same report, against what runs: the engine service is Ready, runs as its own account with internal ingress,
nobody public may invoke it, `/health` answers, the running image carries the vendor's cosign signature, the
bucket has versioning and uniform access with public access prevented, the dataset has no public entry, and the
deployment credential is accepted by the control plane.

## Steps 2 and 3

Step 2 is in [`deploy/iam/gcp/byoc/README.md`](../../deploy/iam/gcp/byoc/README.md) (or `byovpc`); every
permission it grants, who holds it, why, and what breaks without it is in [privileges.md](../privileges.md).
Step 3 is in [`deploy/gcp/README.md`](../../deploy/gcp/README.md), "Production". The template's `terraform_vars`
output is what step 3 takes, so the install adopts your bucket and dataset and runs on the vendor role alone (see
[Installing as the vendor role alone](../privileges.md#installing-as-the-vendor-role-alone)).

## What the vendor can and can't do

- Install and upgrade, as the management identity your template created, through the engine's agent: it pulls a
  signed desired state and runs only images signed with the key you were given.
- Never read or write the memory, read a secret's value, read table data, delete a data store, or change IAM.
- Open a break-glass support session, read-only, that is written to your engine's audit log
  (`ampup-memory audit --operator --kind support`).
- Be revoked: `terraform apply -var vendor_access_enabled=false`. The engine, its identities and the data stay.

## The rest of this directory

| Page | Read it when |
|---|---|
| [preflight.md](preflight.md) | you want to know what each check reads and when it fails (generated from the code) |
| [sizing.md](sizing.md) | choosing a size, filing a quota increase, estimating cost |
| [network.md](network.md) | your network team asks what goes out, what comes in, and what a BYOVPC subnet needs |
| [operations.md](operations.md) | upgrades, rollback, channels and windows, audit, monitoring, secrets |
| [vendor-access.md](vendor-access.md) | a security review asks what the vendor can do, what leaves, and how to revoke |
| [offboarding.md](offboarding.md) | taking AmpUp Memory out, in order, and what stays |
| [troubleshooting.md](troubleshooting.md) | something failed: what you see, why, what to do |
