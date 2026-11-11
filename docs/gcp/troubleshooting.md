# Troubleshooting

The failures seen in real installs, each with what you see, why, and what to do. Most are caught earlier by
`ampup-memory preflight gcp` (the check's id is given); run it first, and `--installed` after.

| What you see | Why | Do | Preflight |
|---|---|---|---|
| `terraform apply` of the template refuses to plan: `dedicated_project` must be true | Workload Identity trusts the engine's identity in every cluster of the project | confirm the project is AmpUp Memory's alone and set `-var dedicated_project=true` | `project` |
| The template's apply fails with `Editor` refused on a service account binding | Editor can't grant on service accounts | apply as Owner, or add Project IAM Admin, Role Administrator and Service Account Admin | `permissions` |
| A grant to the vendor principal fails: "not a member of the allowed domains" | domain-restricted sharing | an org-policy exception on the project for `iam.allowedPolicyMemberDomains` with the vendor's customer id | `org-policy` |
| A first install fails with a 403 from BigLake | new IAM bindings take up to a minute to propagate | apply again a minute later | |
| The engine answers 403 to everyone for a few minutes after install | it refuses callers until it has confirmed Cloud Run IAM verifies them (up to 5 minutes) | wait; `--installed` shows `health` 200 once it does | `health` |
| The IAM template or the install module stops at a custom role: "already exists" or the id is reserved | GCP keeps a deleted custom role's id for 37 days | `-var name=<another prefix>`, or wait | `names` |
| `--installed` says the engine runs as another account or ingress `all` | an install made by hand, or an older module | apply the module again | `engine` |
| `signature_missing` from the agent, or `--installed` FAIL on `signature` | the image isn't signed with the key you pinned | the agent applies nothing: the vendor rolls a signed image; check the key's fingerprint against the one on your org page | `signature` |
| The console shows the deployment stale | the agent or engine hasn't reported in 10 minutes | egress to the control plane from the engine's subnet ([network.md](network.md)); on Cloud Run, the engine reports itself, so check its logs | `egress`, `enrolled` |
| `/v1/whoami` answers 401 for the deployment | the credential was revoked or rotated elsewhere | enroll again with a new token from the console | `enrolled` |
| The first request after idle takes 5 to 15 s (trial) | the engine scales to zero and loads its index on the first request | `-var min_instances=1`, or the team size | |
| 429 "no available instance" on a bulk load (trial) | one instance, one vCPU | retry with backoff, or load with the team size's load job | |
| Writes refused with 403 past 1,000 episodes (trial) | the trial's cap | the team size | |
| `bq show` or the dataset row says the dataset is missing | the template was applied without `cloud_run = true` | apply again with it | `data` |
| `gcloud quotas info list` is refused | the Cloud Quotas API isn't enabled or you lack `cloudquotas.quotas.get` | the row is SKIP; enable `cloudquotas.googleapis.com` or ask someone with Viewer to run it | `quota` |
| Cloud NAT exists but nodes still can't reach the registry | the subnet isn't in the NAT's range, or Private Google Access is off | add the subnet to the NAT; turn Private Google Access on | `egress` |

## Getting help

Run the preflight with `--json` and attach the file to the ticket; it has no memory, names or secrets in it,
only what each check found. For a running install, make a bundle (`ampup-memory support-bundle`, see
[operations.md](operations.md)), read it, and attach it. Or the vendor may ask to open a support session, which
one of your admins approves: what it can and can't read is in [vendor-access.md](vendor-access.md), and you see
every command in `ampup-memory audit --operator --kind support`.
