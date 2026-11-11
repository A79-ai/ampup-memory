# Operations: upgrades, rollback, policy, audit

You decide when the engine changes. The vendor publishes releases; your policy says which channel you follow,
whether an upgrade waits for you, and when it may run. The agent in your project applies only what the control
plane signed, and only an image signed with the vendor's key you were given.

## Policy

Set it in the control plane's console, as an admin of your org:

| Setting | Values | Means |
|---|---|---|
| channel | `canary`, `early`, `ga` | which releases you're offered; `ga` is the default |
| update mode | `manual`, `auto` | `manual`: an upgrade waits in the console for you to approve; `auto`: it runs in the next window |
| windows | `DAYS@HH:MM+HOURS`, several allowed | when an automatic upgrade may start (UTC, for example `sat,sun@02:00+4`); none means any time |
| pinned | `on`, `off` | freeze the version; the console still shows what's available |
| actions | `on`, `off` | whether the vendor may ask the agent to run its allowlisted actions (restart, reindex, drain) |
| support | `on-request`, `off` | `on-request`: an engineer asks for a read-only session and one of your admins approves; `off`: none can be asked for, and the agent refuses anything sent |

## Upgrading

**Cloud Run today:** the deployment agent runs on Kubernetes only, so on Cloud Run the policy is recorded but not
acted on. An upgrade is you applying the install module again with the new `image` the console offers, and a
rollback is the same with the previous one; Cloud Run keeps the old revision.
The engine still reports its state and version to the console, so "stale" and "offered update" are shown. A
Cloud Run agent target is in the plan.

**GKE:** an upgrade replaces the engine's image at the next window (auto) or when you approve (manual). The agent reads
the new image's signature first; an image not signed with the vendor's key is refused and reported, whatever the
control plane says. The previous image stays in the registry for a rollback. The console shows the version
running and its history, and rolls back to the previous generation.

Rollback is the previous generation's image; the memory is unchanged (the engine reads an append-only ledger
and the index is rebuilt from it if the layout changed). After any change, `ampup-memory preflight gcp --installed`
is the check.

## Audit

Two logs, both yours:

- **The engine's audit**, in your bucket, written by the engine and the agent: every upgrade, rollback, action,
  support session and command, and each admin change of a schema or an org. Read it as the operator with the
  admin key: `ampup-memory audit --operator`, or `--kind support` for support alone.
- **Cloud Audit Logs** in your project record what the vendor's management identity did through GCP IAM:
  filter `protoPayload.authenticationInfo.principalEmail="<vendor management service account>"`.

The control plane keeps its own audit of what was asked (in the console); it
records the request, your engine's audit records what ran.

## Monitoring

Cloud Monitoring has the service's request count, latency and instance count; alert on 5xx rate and on
`container/memory/utilizations` near 1. The engine's `/metrics` (Prometheus) is readable with the admin key
inside the project. The console shows what the agent last reported and marks a deployment stale after 10
minutes without a report.

## Secrets

The engine reads the model key from Secret Manager under the `memstore-` prefix. Rotate it by adding a new
version; the engine reads the latest at start and on the agent's `restart` action. The vendor's identity may
write `memstore-*` secrets (to roll a key it manages for you) and can't read any secret's value.

## Support

Read-only, time-boxed, approved by your own admins, written to your audit. What a support session may run, how
to see one, end one or turn them off: [vendor-access.md](vendor-access.md).

For a ticket, make a bundle yourself and read it before you send it:

```sh
ampup-memory support-bundle --engine <engine URL> --admin-key-file ops.key \
  --project <project> --region <region> --service <name>-engine --since-minutes 120
# or, on GKE: kubectl -n memstore logs deploy/memstore --since=2h > engine.log; … --logs-file engine.log
```

It writes `memstore-bundle-<time>/` here: `health.json`, `telemetry.json`, `metrics.json`, `index_status.json`,
`config.json` (what a support session's diagnostics read, through the same views), `logs_summary.json` (counts by
severity and by ten minutes), `logs_errors.txt` (the error and warning lines, at most 500, each redacted) and
`manifest.json`. Without the admin key it holds health and logs only. Nothing is uploaded.
