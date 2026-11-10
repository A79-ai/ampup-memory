# Network

Nothing of the vendor's connects into your project. Everything memstore does on the network is outbound from
the engine and its agent, or inside Google's own network between your services.

## What goes out, and to where

| From | To | Port | Why | Stop it and |
|---|---|---|---|---|
| the agent | the control plane (`--control-plane`, the URL the vendor gave you) | 443 | pulls the signed desired version and policy every 30 s; posts the engine's state and the telemetry contract's fields, nothing else | upgrades, rollback, support and the console's health view stop; the engine keeps serving |
| the agent and Cloud Run | the image registry (`--image-registry`, e.g. `us-docker.pkg.dev`) | 443 | pulls the engine image and reads its cosign signature | upgrades stop at the current image |
| the engine | Google APIs: `storage`, `biglake`, `bigquery`, `aiplatform`, `secretmanager`, `cloudidentity` (`*.googleapis.com`) | 443 | the memory, the catalog, the SQL surface, extraction, the model key, group lookups | the engine doesn't start |
| the engine | your model provider, when you bring a key (`api.anthropic.com` or `openrouter.ai`) | 443 | extraction | extraction stops; search goes on |
| the engine | the control plane | 443 | the telemetry contract's fields (`memstore telemetry schema` prints every one) | the console shows the deployment stale |

The exact control-plane and registry hosts are on the page the vendor gives you with your org. `memstore
preflight gcp --control-plane … --image-registry …` tries both from your shell; the nodes need the same path.

## What comes in

- **Cloud Run:** the engine's ingress is `internal`, and only principals with `roles/run.invoker` on it may
  call it: your BigQuery connection's service account (for the SQL functions), and the users or groups you grant
  for MCP. `memstore preflight gcp --installed` fails if `allUsers` or `allAuthenticatedUsers` is among them.
- **GKE:** the engine is a ClusterIP service behind an internal load balancer; the thin Cloud Run proxy for
  BigQuery reaches it over Direct VPC egress. No public address.
- The vendor's support session is not an inbound connection: the agent polls for it and runs read-only
  diagnostics itself ([vendor-access.md](vendor-access.md)).

## BYOVPC: what your subnet needs

- One subnet in the region, `/24` or larger, in your VPC or a Shared VPC host project (`network_project`).
- **Private Google Access** on, so private nodes reach Google APIs without external addresses.
- **Cloud NAT** (the BYOC template makes one; BYOVPC uses yours) or an egress proxy that passes 443 to the
  control plane and the registry. With a proxy, set `HTTPS_PROXY` on the agent's workload.
- Firewall: allow egress 443 to those hosts and to `199.36.153.8/30` (`private.googleapis.com`) if you restrict
  Google API ranges; allow the GKE control plane to reach nodes (the template's rules).
- Nothing inbound from outside the VPC.

## Org policies that touch the network

`compute.vmExternalIpAccess` and `compute.requireShieldedVm` may stay enforced: nodes get no external IP and are
shielded. `compute.restrictVpcPeering` is fine: no peering is made. `run.allowedIngress` must allow `internal`.
The preflight reads all four.
