# The deployment agent

`memstore agent` keeps a customer's engine at the version its control plane wants and runs the few
actions the control plane may ask for. It is a process of its own: the engine never upgrades itself.
Like everything else a deployment does, it only calls out. Every 30 seconds, give or take a fifth, it
makes three signed requests. `GET /v1/desired` returns the generation to run: an image tag and digest,
feature flags, and the customer's policy. `GET /v1/actions` returns the actions waiting.
`GET /v1/support` returns the support session open, if any (see [Support sessions](#support-sessions)).

It acts only on what the control plane's own Ed25519 key signed. The key is pinned in the credential
when the deployment enrolls, or given as `MEMSTORE_CONTROL_PLANE_KEY`. With neither, the agent refuses to
start.

`--trust-control-plane-key-on-first-use` (`MEMSTORE_AGENT_TOFU=1`) is an opt-in for credentials
enrolled before the control plane gave its key. With it, the agent takes the key `/v1/whoami` gives and
keeps it in its state. It logs a warning with that key's SHA-256 fingerprint, which you should compare
with the one your control plane publishes. The kept key counts only while the opt-in stays on.

The agent compares whoami's key with the pinned one at start and about every 10 minutes. If they
differ, the agent logs both fingerprints and acts on nothing more.

A payload must also name this deployment and be at most 10 minutes old. A generation older than one the
agent already acted on is never applied.

## Image signatures

The control plane's signature says which image to run; it doesn't make the image a trusted release. Before
the agent sets `<repository>:<tag>@<digest>`, it checks that the digest carries a cosign signature made by a
key pinned in the agent. A control plane that is compromised can't make it run an image the release key
didn't sign.

Images are signed with cosign v2 in its classic format, with a Cloud KMS `EC_SIGN_P256_SHA256` key
and no transparency log:

```sh
cosign sign --key gcpkms://projects/P/locations/L/keyRings/R/cryptoKeys/K/cryptoKeyVersions/1 \
  --tlog-upload=false us-docker.pkg.dev/P/memstore/engine@sha256:<digest>
```

That writes a manifest tagged `sha256-<hex>.sig` beside the image. With `COSIGN_REPOSITORY` set when
signing, the manifest goes to that repository instead, and the agent must be told the same
repository with `MEMSTORE_AGENT_SIGNATURE_REPOSITORY` (or `--signature-repository`). It defaults to
the image repository (for example
`COSIGN_REPOSITORY=REGION-docker.pkg.dev/PROJECT/SIGNATURES-REPO/memstore`). The agent
reads both repositories with the same registry auth, and the payload's
`critical.identity.docker-reference` still names the image repository. Each of its layers is a JSON
payload naming the image's digest, with its signature in the annotation
`dev.cosignproject.cosign/signature`. The agent reads it over the OCI distribution API. The image
counts as signed when at least one layer meets all of these:

- the blob is the one its descriptor's digest names;
- its signature (DER or raw) verifies with the pinned key;
- its `critical.type` is `cosign container image signature`;
- its `critical.image.docker-manifest-digest` is exactly the digest the control plane named;
- its `critical.identity.docker-reference` is exactly `MEMSTORE_AGENT_IMAGE_REPOSITORY`. cosign v2
  writes the image's repository there, with no tag or digest, even when you sign by tag or with
  `COSIGN_REPOSITORY`. A signature made with `--sign-container-identity` names something else and is
  refused.

Give the agent the public key as a PEM file with `MEMSTORE_AGENT_COSIGN_KEY` (or `--cosign-key`). For
a Cloud KMS key:

```sh
gcloud kms keys versions get-public-key 1 --key=K --keyring=R --location=L --project=P \
  --output-file=cosign.pub
```

The file is a PEM `PUBLIC KEY` block (EC P-256). The agent logs the key's fingerprint at start. What it reports when the check fails:

| Code | What happened | Tried again? |
|---|---|---|
| `signature_missing` | The repository has no `.sig` for the digest | No, like a rollback: nothing was applied, and the phase is `failed` until a newer generation |
| `signature_invalid` | There are signatures, but none is the pinned key's over a payload for this digest | No, the same |
| `registry_unreachable` | The registry couldn't be read (down, or it refused the agent's credentials) | Yes, next round |
| `signature_unverifiable` | No key is pinned | Each round, until a key is pinned; then it is checked and applied |

With no key, the agent applies no upgrade, but actions still run. `--allow-unsigned-images`
(`MEMSTORE_AGENT_ALLOW_UNSIGNED=1`) turns the check off when no key is pinned. It logs a warning at
start and on every upgrade. It is for a registry you can't sign in yet, not for production.

### Security notes

- **Key scope.** The pinned key must sign only this repository's release images: no debug builds,
  no other products, no other customers' images. The repository check above stops a signature made
  for another repository from being used here. But any image the key signed in this repository, the
  agent will run when the control plane names it.
- **Earlier releases.** The agent has no minimum version, on purpose. Rolling back to an earlier
  signed release is intended, and the control plane orders it with a newer generation of its signed
  desired state. Generations only move forward: the agent never applies one older than one it acted
  on, so an old desired state replayed can't take the engine back. A signature only says which
  images may run. The control plane's signed generation says which one runs now.

How the agent reads the registry (`MEMSTORE_AGENT_REGISTRY_AUTH`):

| Value | How |
|---|---|
| `gcp-metadata` | Default for `*-docker.pkg.dev` and `gcr.io`. An access token from the GKE metadata server, sent as a bearer token, and as the password of the registry's token handshake if it asks for one. The agent's Kubernetes service account needs Workload Identity to a Google service account with `roles/artifactregistry.reader` on the signature repository |
| `anonymous` | Default for any other registry. The registry's anonymous token handshake (`WWW-Authenticate: Bearer`), for a public repository |
| `none` | No authorization |

## On Kubernetes

[`k8s/agent.yaml`](k8s/agent.yaml) has a ServiceAccount and a Role that allow `get` and `patch` on the
engine's one Deployment (`resourceNames`) and nothing else. It also has a small volume for the agent's
state and a one-replica agent Deployment. Change the namespace, names, image and repository to yours,
then `kubectl apply -f deploy/agent/k8s/agent.yaml`.

The public key is a ConfigMap mounted at `/etc/memstore-cosign` (a public key isn't a secret). For
`gcp-metadata`, annotate the ServiceAccount with the Google service account it acts as
(`iam.gke.io/gcp-service-account`), and give that one `roles/artifactregistry.reader` on the repository the signatures are in.

The agent runs as its own Deployment, not as a sidecar in the engine's pod. Upgrading the engine
replaces the engine's pods, and with a rolling update the old and new pods run side by side, so a
sidecar would be killed mid-rollout and would briefly run twice. Its strategy is `Recreate`, so two
agents never run at once.

| Setting | Meaning |
|---|---|
| `MEMSTORE_DEPLOYMENT_CREDENTIAL` | The credential `memstore deployment enroll` wrote (the engine's own, as a Secret) |
| `MEMSTORE_AGENT_IMAGE_REPOSITORY` (required) | The only repository the engine may run from. The control plane names a tag and a digest, and the agent sets `<repository>:<tag>@<digest>` |
| `MEMSTORE_AGENT_NAMESPACE`, `_DEPLOYMENT`, `_CONTAINER` | The engine's workload (default: the agent's namespace, `memstore`, `memstore`) |
| `MEMSTORE_AGENT_HEALTH_URL` | The engine's health (default `http://<deployment>.<namespace>.svc:8080/healthz`) |
| `MEMSTORE_AGENT_HEALTH_TOKEN_FILE` | An admin key: with the health URL set to the engine's `/telemetry`, the state the exporter would send must say `ok` |
| `MEMSTORE_CONTROL_PLANE_KEY` | The control plane's key (base64), when the credential doesn't pin one |
| `MEMSTORE_AGENT_TOFU` | `1`: with no key pinned, trust whoami's on first use (see above) |
| `MEMSTORE_AGENT_STATE` | The agent's state (default: `agent.state.json` beside the telemetry spool) |
| `MEMSTORE_TELEMETRY_MIRROR` | The engine's telemetry mirror on a volume the agent shares. This lets `drain_spool` and `clear_spool` act |
| `MEMSTORE_AGENT_POLL_SECONDS`, `_ROLLOUT_TIMEOUT_SECONDS`, `_HEALTH_TIMEOUT_SECONDS` | 30, 1200, 120. Keep the rollout timeout above the deployments' `progressDeadlineSeconds` and the engine's warm-up |
| `MEMSTORE_AGENT_DRIFT_MODE` | `report` (default) or `repair`: report drift, or restore the accepted generation inside its maintenance windows |
| `MEMSTORE_AGENT_EMBED_PROVIDERS` | Comma-separated providers the agent may choose; empty by default. Match the admission policy's `embedProviders` list, including the original provider for rollback |
| `MEMSTORE_AGENT_TARGET` | `kubernetes`. `cloud-run` and `snowflake` are not built yet |
| `MEMSTORE_AGENT_COSIGN_KEY` | A PEM P-256 public key: images must carry a cosign signature by it (see above) |
| `MEMSTORE_AGENT_ALLOW_UNSIGNED` | `1`: with no key, apply images with no signature check (warned of) |
| `MEMSTORE_AGENT_REGISTRY_AUTH` | `gcp-metadata`, `anonymous` or `none` (default by the registry's host) |
| `MEMSTORE_AGENT_SIGNATURE_REPOSITORY` | Where the `.sig` manifests are, when images were signed with `COSIGN_REPOSITORY` (default: the image repository) |
| `MEMSTORE_AGENT_ENGINE_URL` | The engine, for support diagnostics (default: the origin of the health URL) |
| `MEMSTORE_AGENT_ADMIN_KEY_FILE` | The operator's admin key for the engine. Support diagnostics read `/telemetry` and `/metrics` with it, and support sessions are written to the engine's audit with it (see [Support sessions](#support-sessions)) |
| `MEMSTORE_AGENT_SUPPORT_POLL_SECONDS` | How often an open support session is looked at (5) |

### What the agent may change

RBAC can't limit a `patch` to some fields: with `patch` on the engine's Deployment alone, an agent
that was taken over could change anything in the engine's pod, such as its command, volumes or
service account. So the manifest also has a ValidatingAdmissionPolicy and its binding with
`validationActions: [Deny]`. The API server refuses any request from the agent's service account
other than an update of the engine's Deployment itself that changes only these:

- the `memstore` container's image, from your repository and pinned by digest;
- its `MEMSTORE_FLAG_*` variables, with the literal value `0` or `1`;
- `MEMSTORE_EMBED`, `MEMSTORE_EMBED_MODEL` and `MEMSTORE_EMBED_DIMS`, only when the operator allows
  embedding changes in both the agent and admission policy, with literal, bounded values;
- the pod template's `kubectl.kubernetes.io/restartedAt` annotation.

The policy names every field of DeploymentSpec, ObjectMeta, PodSpec and Container as of Kubernetes
1.35, and requires each to be as it was. It can't loop over a typed object's fields: CEL's `all()`
over one sees no fields, so it would pass anything. Change the namespace, the names and the
repository in its image pattern to yours, as in the rest of the manifest. Three things to know:

- When the cluster moves to a Kubernetes version that adds fields to those types, add them to the
  policy. A field it doesn't name, the agent could change.
- Pin the engine's first image by digest (`<repository>:<tag>@sha256:…`). A rollback patches the
  previous image back in, and the policy refuses an image that isn't pinned by digest. A first image
  pinned only by tag would leave a failed upgrade with no way back.
- Guard `request.subResource` with `has()` in the policy. It is unset for a request on the Deployment
  itself, so reading it without `has()` is an error, and with `failurePolicy: Fail` that error
  refuses every patch.

Embedding changes are off by default. To allow them, set `MEMSTORE_AGENT_EMBED_PROVIDERS` on the agent
and the policy's `embedProviders` list to the same providers (`local`, `openai`, `openrouter`). Include
the existing provider so rollback can restore it. The policy accepts a model of 1–128 letters, digits
or `/ . _ : -`, and dimensions from 1 to 8192. Secret references, API URLs, keys and every other
variable remain immutable. Removing all embedding variables restores the deployment's implicit default;
when allowing embedding changes, that default must also be acceptable to the customer. The agent's
service account cannot change this policy.

## An upgrade

1. Outside the customer's windows (UTC, Sunday is day 0, and a window may run past midnight or into
   Sunday), the agent reports `waiting_window` and does nothing, unless the generation carries
   `apply_now: true` (the customer's admin installed it from the console), which goes ahead at once.
   Every other step below still applies to it.
2. It checks the image's signature (see above). If the check fails, it applies nothing and reports
   `failed` with the code.
3. It reports `applying`. In one strategic merge patch it sets the engine container's image and its
   `MEMSTORE_FLAG_<NAME>=1|0` variables, and removes flag variables the generation no longer names.
4. It waits until the rollout is complete: `observedGeneration` has caught up, and every wanted replica
   is updated and available, with no old one left. Then the engine must answer healthy.
5. Then it reports `healthy`. If the rollout times out or stalls (`rollout_timeout`, `rollout_stalled`),
   or the engine isn't healthy (`health_failed`), it patches the previous image and flags back and
   reports `applying` with `rollback_in_progress` while it restores the previous image and settings.
   It reports `rolled_back` with the original failure code only after the engine and every configured
   cell have rolled out, match the restored settings, and the engine answers healthy. A failed rollback
   reports `failed` with `rollback_failed`, `rollback_timeout`, `rollback_stalled`, `rollback_unhealthy`
   or `rollback_drift`. It does not try a failed or rolled-back generation again; a newer generation
   is required.

The agent records the intended configuration before it patches, and records the rollback phase before
restoring anything. A restart resumes that phase. A partial patch across the engine and cells also
enters rollback. An in-flight record written by an older agent lacks the intended configuration, so
it is conservatively rolled back with `interrupted` rather than certifying whatever happens to run.

## Continuous reconciliation

After a generation becomes healthy, each signed poll compares its accepted image and managed settings
with the engine and every configured cell. A changed payload with the same generation cannot replace
the accepted configuration. An older healthy state file is initialized from the signed desired state.
Flags are authoritative: extra flags count as drift. Embeddings omitted from the accepted specification
remain operator-managed.

`MEMSTORE_AGENT_DRIFT_MODE` (`--drift-mode`) is local to the agent:

- `report` (default): report `failed` / `drift_detected` without patching any workload.
- `repair`: restore the accepted specification inside the current maintenance windows. Outside them,
  report `waiting_window` / `drift_detected`. A past `apply_now` does not bypass windows for later
  repairs. Before repairing, recheck the image signature and the local embedding-provider allowlist.

A repair reports `applying` / `drift_repair`, then `healthy` only after rollout, health and configuration
checks pass. A failed repair stops that generation until a newer one arrives; it never rolls back to
the drifted configuration. If the configuration matches but the deployment is still rolling out or the
engine is unhealthy, its reported status reflects that observation instead of repeating an old healthy
status. Report-only drift clears when the operator restores the accepted configuration.

The status wire format is unchanged: recovery uses the existing `applying` phase with a bounded code,
so this agent can run against the existing control plane.

## Actions

Only these run, each id at most once. A result that never reached the control plane is sent again,
and the action is not run again. Results are an outcome (`ok`, `failed`, `refused`), a code and counts.
They never include text.

| Kind | What the agent does |
|---|---|
| `restart` | Sets the `kubectl.kubernetes.io/restartedAt` annotation on the pod template |
| `reindex {scope}` | Refused, code `unsupported`: the engine has no admin endpoint for it yet |
| `drain_spool` | Sends the telemetry spool now (counts `sent`, `pending`). This needs `MEMSTORE_TELEMETRY_MIRROR`; without it the action is refused as `unsupported` |
| `clear_spool` | Drops what the spool holds; the mirror keeps its copy (count `cleared`). Same requirement |
| `rotate_key` | `memstore deployment rotate` on the credential. It needs a writable credential that the engine reads too. With a read-only Secret it fails with code `rotate_failed`; rotate by hand and update the Secret |
| `diagnostics` | Counts only: generation, health, remembered actions, replicas wanted, updated and available, and spool size |

The agent refuses any other kind, or arguments that don't match the kind (`unknown_kind`, `bad_args`). It
runs no actions while the customer's policy turns them off.

## Support sessions

A support session is break-glass access for the vendor's engineers: read-only diagnostics, asked for through the
control plane and approved there for one deployment for a limited time. The agent asks
`GET /v1/support` (a signed request) every round, and every 5 seconds while a session is open. The
answer is checked like the desired state and the actions: it must be signed by the pinned control plane
key as the `support` kind (`memstore-support\n<deployment id>\n<payload>`), name this deployment, and be
at most 10 minutes old. A forged, replayed or stale payload runs nothing.

The customer's policy comes first. With `support: off` in the desired state's policy (set in the control
plane's console), the agent answers every command of any session it is handed with
`refused`, code `support_off`, logs the session as refused by policy and audits it the same way, so both sides
see it was asked and not run; the control plane refuses to open a session for that deployment too. `on-request`
(the default, and what a control plane from before the setting means) is a session an engineer asked for and one
of the customer's own admins approved.

A session lists commands, each a diagnostic by name. Each command id runs at most once: the agent keeps
the ids and results in its state, and posts the result to `POST /v1/support/<id>/result` again until the
control plane has it (a 404 means it has). A command is refused with code `expired` once the session's
`expires_at` has passed; the expiry is checked as each command starts. Results are an outcome (`ok`,
`failed` or `refused`), a code, and an output object of at most 64 KB. A bigger output has entries
dropped from its largest map and `"truncated": true` added; if it still doesn't fit, it isn't sent
and the result is `failed` with code `output_too_large`.

| Diagnostic | What it returns | Engine endpoint |
|---|---|---|
| `health` | `/healthz`'s status and `ok`; with an admin key, the state the telemetry exporter builds (health, conditions, version, uptime, index lag and failures) | `GET /healthz`, `GET /telemetry` |
| `telemetry` | The `state` and `inventory` payloads the exporter would send, read through the telemetry contract's own types (counts and closed sets only) | `GET /telemetry` |
| `metrics` | `{family: value}`: every numeric sample summed by metric name, labels dropped (they can hold org names), histogram buckets left out | `GET /metrics` |
| `index_status` | Index lag in seconds, failed index updates, scopes by kind (orgs, teams, users), and episode and fact rows. Counts only, no scope names | `GET /telemetry` |
| `config` | The engine's version, platform and directory event version, and its container's variables by name. A value is shown only for a fixed set of non-secret settings (platform, caller identity mode, embeddings backend, model and width, feature flags, intervals and limits), and only when it is a plain token. Anything else is `set`; known secrets (API keys, the admin key, JWT and client secrets) are always listed as `set` or `unset` | `GET /telemetry`, and the engine's Deployment through the Kubernetes API |
| `catalog_status` | Refused, code `unsupported`: the engine has no endpoint for table snapshots and row counts yet | none |
| `logs` | Refused, code `unsupported`: the engine has no contract-filtered log, and raw logs are never shipped | none |

Any other name is refused with code `unknown_diagnostic`, before the engine hears of it. Without an
admin key, the diagnostics that need one are refused with `no_admin_key`.

**Never reachable.** The agent makes no other request of the engine: only `GET /healthz`,
`GET /telemetry` and `GET /metrics`, and `POST /admin` with the op `support_event` (below). A support
session can't make it call `/query`, `/search`, `/episode`, `/facts`, `/mcp` or any other endpoint that
reads memory, whatever the control plane sends. No fact, episode, query, source, org, team, scope or user
name, key, secret value or log line is in any output.

**Audit.** The agent logs each session it sees open (who requested and who approved it, until when),
each command (its diagnostic, outcome and code) and each session closed or expired. With
`MEMSTORE_AGENT_ADMIN_KEY_FILE`, it also writes each of these to the engine's own audit log through the
`/admin` op `support_event`. Only the operator's admin key may write it, and only in a closed shape:
`{kind: support_session|support_command, session, command?, diagnostic?, outcome, code?, by}`. The events
belong to no org, so they show in the operator's view:

```console
$ memstore audit --operator --kind support
2026-10-18T07:36:29Z  support  support@example.com  session sup-0123456789abcdef open
2026-10-18T07:36:29Z  support  support@example.com  session sup-0123456789abcdef command cmd-00000000000000d1 health: ok
2026-10-18T07:36:30Z  support  support@example.com  session sup-0123456789abcdef command cmd-00000000000000d2 index_status: ok
2026-10-18T07:36:30Z  support  support@example.com  session sup-0123456789abcdef command cmd-00000000000000d3 metrics: ok
2026-10-18T07:36:30Z  support  support@example.com  session sup-0123456789abcdef command cmd-00000000000000d4 other: refused (unknown_diagnostic)
2026-10-18T07:36:33Z  support  support@example.com  session sup-0123456789abcdef closed
```

The control plane keeps its own record of who requested and approved each session and each command.
