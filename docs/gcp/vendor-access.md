# What the vendor can do in your project

One page, so a security review can read it alone. Every permission with its scope and reason is in
[privileges.md](../privileges.md); this is what they add up to.

## Identities

| Identity | Created by | Used for | Can be revoked by |
|---|---|---|---|
| The management service account (`<name>-vendor@<project>`) | your IAM template | installing and upgrading: creating the engine service and jobs, the catalog, the BigQuery connection and functions; writing `memstore-*` secrets | `terraform apply -var vendor_access_enabled=false` |
| The vendor principal (`vendor_principal`) | the vendor | the only thing it can do in your project is impersonate the management account | the same variable removes the grant |
| The engine account (`<name>-engine@`) | your IAM template | what the engine runs as: the bucket, the catalog, BigQuery, the model | yours; the vendor can't change its grants |
| The agent account (GKE) | your IAM template | reading image signatures, changing the engine Deployment's image | yours |

The vendor never holds a key: it impersonates the management account from its own identity, and that account
has no key either (`iam.disableServiceAccountKeyCreation` may stay enforced).

## What the management identity can't do

- Read or write objects in the memory bucket, read a secret's value, read BigQuery table data, or delete the
  bucket or the dataset (`force_destroy = false`; yours to delete).
- Change IAM: no `setIamPolicy` anywhere. It attaches the runtime accounts to workloads and nothing more.
- Touch anything not named `<name>*`: every project-level grant carries a condition on the resource's type and
  name, and a resource it can't recognise gets nothing.
- Create a GKE cluster by any other name, or a Cloud Run service outside the `<name>-*` set.

## What leaves your project

- **Telemetry**: the fields in the contract, and only those. `ampup-memory telemetry schema` prints every one;
  `ampup-memory telemetry check` refuses a payload with any other. Counts, states, versions, timings. No memory,
  no names of orgs, teams, users or scopes, no log lines.
- **Support diagnostics**, when a session is open: `health`, `telemetry`, `metrics` (summed, labels dropped),
  `index_status` (counts), `config` (variable names; values only for a fixed non-secret set). Never `/query`,
  `/search`, `/episode`, `/facts`, `/mcp` or logs, whatever the control plane sends: the agent refuses by name.
- Nothing else. The engine and the agent only call out ([network.md](network.md)).

## Support sessions

Break-glass, read-only, for a named deployment, approved in the control plane for a limited time. Each session
and each command in it is written to your engine's audit log with who asked, who approved, the diagnostic and
its outcome:

```sh
ampup-memory audit --operator --kind support
```

A session exists only because one of your admins approved it: an engineer asks, the control plane refuses the
vendor's own approval, and nothing runs until your admin says yes. To end one, end it in the console. To refuse support
altogether, set the deployment's support policy to `off` in the console.

No session can then be asked for, and the agent answers anything it is sent with `refused` (`support_off`),
audited in your engine. Back on with `on-request`.

Instead of a session, you can make a **support bundle** yourself and attach it to a ticket: `ampup-memory
support-bundle` writes a directory on your machine with what a session's diagnostics would read (health,
telemetry through the contract's types, metric families with labels dropped, index counts, config names) and a
redacted summary of the engine's logs (counts by severity, the error and warning lines with emails, keys and URL
queries blanked). Its `manifest.json` lists every file and how many spans were redacted. Nothing is sent: you
read it, then send it if you choose ([operations.md](operations.md)).

## Revoking the vendor

```sh
terraform apply -var vendor_access_enabled=false   # with the same -var flags as the install
```

Removes the impersonation grant and disables the management account. The engine, its identities, the bucket,
the dataset and the memory stay, and keep working. To stop upgrades as well on GKE: `kubectl -n memstore scale
deployment/memstore-agent --replicas=0`. Turn it back on with `=true`. Cloud Audit Logs show every call the
management account ever made: filter on its email.
