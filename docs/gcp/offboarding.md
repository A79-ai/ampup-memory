# Offboarding

The order matters: stop writes, tell the control plane, remove the install, then remove the vendor's access.
The memory stays yours until the last step you choose to take.

## 1. Stop ingestion

Pause the schedule so nothing lands while you take things down:

```sh
gcloud scheduler jobs pause <name>-ingest --project <project> --location <region>
```

and stop the connectors or pipelines that write to the inbox or call the engine.

## 2. Export what you want to keep

The memory is Iceberg tables and the engine's ledger in your bucket, readable by any Iceberg reader through the
BigLake catalog, and the `<name>_api` dataset's inbox in BigQuery. Nothing is in a format only memstore reads.
To keep a copy elsewhere:

```sh
gcloud storage cp -r gs://<bucket> gs://<your archive bucket>/memstore-$(date +%F)
```

## 3. Revoke the deployment in the control plane

Revoke the deployment in the console, so it stops expecting it and no upgrade is offered.

## 4. Remove the install

With the module you installed with:

```sh
terraform -chdir=deploy/gcp/terraform destroy <the same -var flags as the install>
```

or, for an install made by the hosted service's onboarding wizard (not part of this repo), delete what it
created: the `<name>-engine` service, the `<name>-load` job, the BigQuery connection and the functions in
`<name>_api`, and the BigLake catalog. Each is labelled
`memstore-install=<name>`:

```sh
gcloud run services list --project <project> --filter labels.memstore-install=<name>
gcloud run jobs list --project <project> --filter labels.memstore-install=<name>
```

The BigLake catalog refuses to delete while it holds tables; empty it first or leave it (it costs nothing).

## 5. Remove the vendor's access, or the whole template

Revoke and keep the data stores and runtime identities:

```sh
terraform -chdir=deploy/iam/gcp/byoc apply -var vendor_access_enabled=false <the same -var flags>
```

Or destroy the template, which removes the identities and roles and **refuses** to delete the bucket and the
dataset (`force_destroy = false`): delete those yourself when you mean to.

```sh
terraform -chdir=deploy/iam/gcp/byoc destroy <the same -var flags>
gcloud storage rm -r gs://<bucket>            # only when you mean to
bq rm -r -d <project>:<name>_api              # only when you mean to
```

## What stays behind

| Left in the project | Why | Remove with |
|---|---|---|
| The bucket and the dataset | yours by design; the template never deletes data | the two commands above |
| The APIs the template enabled | never disabled on destroy (another workload may use them) | `gcloud services disable …` |
| The custom role ids `<name>_vendor_*`, `<name>_engine_*` | GCP reserves a deleted role's id for 37 days | wait, or use another `name` to reinstall |
| Cloud Audit Logs of the vendor's calls | your logs | your retention policy |
| The `memstore-*` secrets | the template leaves what it didn't create | `gcloud secrets delete …` |

A final sweep: `gcloud asset search-all-resources --scope projects/<project> --query 'labels.memstore-install:*'`
lists anything still carrying the install's label.
