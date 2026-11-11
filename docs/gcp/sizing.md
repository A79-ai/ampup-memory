# Sizing

Three sizes. Each names what runs, the quota it takes in the region (by the quota's own name, so an increase can
be filed from the row), and the monthly cost before model use at list prices in `us-central1`. `ampup-memory
preflight gcp` checks the quota of the **team** size unless told otherwise.

| | Trial | Team (default) | Org |
|---|---|---|---|
| Who it's for | one person trying AmpUp Memory for 30 days | a team, always on | several teams, scheduled ingest from your sources |
| Module | `deploy/gcp/trial` | `deploy/gcp/terraform` | `deploy/gcp/terraform` with a bigger engine |
| Engine | Cloud Run, 1 vCPU / 2 GiB, scales to zero | Cloud Run, 2 vCPU / 4 GiB, one instance always on | Cloud Run, 4 vCPU / 8 GiB, one instance always on |
| Load job (first index of a corpus) | none | Cloud Run job, 4 vCPU / 8 GiB, runs on demand | the same |
| Ingest job (scheduled) | none | Cloud Run job, 1 vCPU / 1 GiB, on the schedule you set | the same |
| Episodes | capped at 1,000 | uncapped | uncapped |
| `run.googleapis.com/cpu_allocation` | 1 vCPU | 7 vCPU (2 + 4 + 1) | 9 vCPU |
| `run.googleapis.com/instances` | 1 | 3 | 3 |
| BigQuery | none | the `memstore_api` dataset, remote functions, one connection; slot usage is on demand | the same |
| GCS | one bucket | one bucket, versioned | the same |
| Monthly, before model use | about $1 to $3 at light use | about $111 | about $220 |
| Model use | Claude Haiku: about $10 per 1,000 episodes extracted | BigQuery `AI.GENERATE` (Gemini) or a model key you bring; depends on volume | the same |

What drives cost: the always-on engine instance (about $55 a month of the team's $111), the first load of a
large corpus (the load job runs for the length of the index build), and extraction (one model call per episode).
Search itself is cheap: an index of about 260,000 facts and 34,000 passages searches in tens of milliseconds
from a warm instance.

## GKE instead of Cloud Run

Only when policy forbids Cloud Run. The template's cluster is three `n2-standard-4` nodes in one zone by
default: `N2_CPUS` 12, `SSD_TOTAL_GB` 300, `IN_USE_ADDRESSES` 2, plus a Cloud NAT. Autopilot bills a minimum per
pod; a cluster of your own costs about $300 a month before AmpUp Memory runs on it. `ampup-memory preflight gcp --runtime
gke` checks those three quotas with usage.

## When to move up a size

- Search p95 over 2 s from a warm instance, or the engine's memory at its limit in Cloud Monitoring: the Org
  engine.
- More than one ingest source on a schedule: the Org size, so a long ingest doesn't hold the next.
- A corpus over about 5 GB of text: talk to the vendor first; the index build needs the load job's memory
  raised.
