# Self-hosting

AmpUp Memory stores everything in Apache Iceberg tables in a catalog you choose, over object storage you own. The
server is one binary, `ampup-memory serve`. Search indexes are Lance datasets derived from the tables.

## Catalogs

`AMPUP_MEMORY_PLATFORM` picks the catalog. Every non-local platform speaks the Iceberg REST catalog protocol; an adapter
only supplies the address, auth and storage properties.

| `AMPUP_MEMORY_PLATFORM` | Catalog | Settings |
|---|---|---|
| (`serve --local`) | a JSON catalog file under one directory | `--local [DIR]`, default `~/.ampup-memory` |
| `local` | in-memory catalog over a local warehouse | `AMPUP_MEMORY_WAREHOUSE` |
| `rest` | any Iceberg REST catalog (Apache Polaris, the Iceberg REST fixture, …) | `AMPUP_MEMORY_REST_URI`, `AMPUP_MEMORY_WAREHOUSE`, `AMPUP_MEMORY_REST_PROPS` |
| `gcp` | BigLake metastore Iceberg REST over Cloud Storage | `GCP_PROJECT`, `GCS_BUCKET` |
| `snowflake` | Snowflake Horizon Iceberg REST, vended credentials | `SNOWFLAKE_ACCOUNT`, `SNOWFLAKE_DATABASE`, `SNOWFLAKE_ROLE`, `SNOWFLAKE_PAT` |
| `databricks` | Databricks Unity Catalog Iceberg REST | `DATABRICKS_HOST`, `DATABRICKS_CATALOG`, `DATABRICKS_TOKEN` or `DATABRICKS_CLIENT_ID` + `DATABRICKS_CLIENT_SECRET` |

Storage is resolved per table location: `gs://`, `s3://`, `abfss://` (Azure Data Lake Storage Gen2) and `file://`.
A secret variable can also be given as a file: `<VAR>_FILE` (for mounted secrets) where the code reads it that way
(`SNOWFLAKE_PAT`, the `DATABRICKS_*` credentials).

On BigQuery or Snowflake, the same tables are readable by their own engines.

### Laptop: `serve --local`

```bash
ampup-memory serve --local                 # ~/.ampup-memory, http://127.0.0.1:8080
ampup-memory serve --local ./store org 8090   # another directory, scope and port
ampup-memory service install                 # or keep it running in the background (cli.md)
```

- `DIR/warehouse` holds the tables, `DIR/index` the search index, `DIR/embeddings` the embedding cache, and
  `DIR/catalog.json` names each table's current metadata file, so a restart picks up where it stopped.
- No Docker, no catalog service, no sign-in. Every request reads as one caller in group `local` (or the groups
  given after the port). It binds to 127.0.0.1 and refuses to start if `AMPUP_MEMORY_SERVE_BIND` names anything else;
  requests whose `Host` or `Origin` isn't this machine are refused.
- Look at your memory: open `http://127.0.0.1:<port>/studio/` (or run `ampup-memory studio`). The Studio is built
  into the binary; one built from source without Node.js says how to build it in
  (`npm --prefix studio ci && npm --prefix studio run build`, then `cargo build` again). Other servers don't serve it.
- One process owns a directory at a time. Commands with `--local DIR` (`schema set`, `runs load`, `compile`,
  `advice`, `extract`, `load`, …) need the server stopped.

#### Sources `serve --local` reads by itself

`ampup-memory sources add github OWNER/REPO [--since DATE] [--dir DIR]` names a repository in `DIR/sources.json`.
The server reads that file every minute and each source every 10 minutes, with the token this machine already
holds (`GITHUB_TOKEN` or `GH_TOKEN` in the server's environment, else `gh auth token`; a public repository is read
with none, at GitHub's lower rate). The token is never stored. Each issue and pull request is written as of its last
update (source `github:OWNER/REPO#N`, the hosted connector's format) through the same path as `/ingest`, readable by
the server's groups. The first read goes back 30 days unless `--since` says otherwise; later reads start an hour
before the newest change read (the cursor in `DIR/sources-state.json`), and once a day four days before it, since
GitHub lists a few records under an earlier time than they show. What's held already as sent isn't written again. `sources list` shows each source's records read, last read and last error; `sources remove` stops
reading one (what it read stays).

`ampup-memory sources add github-prs OWNER/REPO` reads the same list but keeps only the merged pull requests, each
read whole through GitHub's GraphQL API, in the hosted GitHub PR connector's format: one episode for the pull
request (source `github:OWNER/REPO#N`: description, files changed, reviews, discussion) and one for each review
thread a person started (`github:OWNER/REPO#N/thread/ID`: file and line, resolved or outdated, the diff hunk, the
comments), dated at the merge. Each replaces its source's earlier versions, so a pull request read before it merged
is replaced by its merged version. Bots' pull requests, reviews and comments, and threads a bot started, are left
out; to leave out a review bot of your own that posts from a person's account, set
`AMPUP_MEMORY_GITHUB_REVIEW_BOT` to how its review bodies start. GraphQL needs a token, even for a public
repository. A pull request GitHub can't give whole, twice, is written as the issues list shows it. A round stops
early, and goes on next round, when GitHub says to wait or fewer than 100 of its hourly points are left. With both
`github` and `github-prs` on one repository, `github` leaves merged pull requests to `github-prs`.

`ampup-memory sources add git PATH [--since DATE] [--dir DIR]` names a local repository instead: the path is kept as
its work tree's top directory, absolute. The server reads it with the `git` on this machine (`PATH`, else
`/usr/bin`, `/opt/homebrew/bin` or `/usr/local/bin`): no network, no token. Each commit reachable from the checked-out
`HEAD`, merges left out (a merge's diffstat repeats its parents'), is one episode: its message, author, date and the
files it touched with lines added and removed, never the diff itself. Its source is `git:OWNER/REPO@SHA` when the
`origin` remote is on GitHub (about `repo:OWNER/REPO`, as the GitHub source writes it, and the pull request a
squash-merged subject ends with, `(#N)`), else `git:DIRNAME@SHA`. The first read goes back 30 days unless `--since`
says otherwise; later reads take only the commits after the last one read (`CURSOR..HEAD`), at most 5,000 a round.
If that commit is gone (rewritten history, then garbage collection), the next read starts from `--since` again, and
what's held already as sent isn't written again. Commits later rebased away stay in memory beside their rewrites.

`ampup-memory sources add linear WORKSPACE/TEAM [--since DATE]` names a Linear team: the workspace's URL key, as in
its address, and the team's key (`acme/ENG`). The server reads it with a personal API key in its environment,
`LINEAR_API_KEY` (from Linear's settings, Security & access); the key is never stored, and a key for another
workspace, or one that can't see the team, is refused with the reason in `sources list`. Each issue is written with
its comments (people's, not bots') and the merged GitHub pull requests linked to it (source
`linear:WORKSPACE/ENG-123`, the hosted connector's format), and written again as a new version, retiring the last,
whenever that text changes. Later reads start an hour before the newest change read; a new comment or a linked pull
request counts as a change to its issue. A server a session starts by itself gets `LINEAR_API_KEY` from that
session's environment; the background service doesn't (no key goes into its file), so run `serve --local` with the
key set to read Linear.

`ampup-memory sources add sentry ORG/PROJECT [--since DATE] [--dir DIR]` names a Sentry project. The
token is `SENTRY_AUTH_TOKEN` in the server's environment, else the file `AMPUP_MEMORY_SENTRY_TOKEN_FILE` names, else
the one `sentry-cli login` keeps in `~/.sentryclirc`; it needs to read the project, its issues and their events (a
personal token or an internal integration's; an organization token made for uploading releases can't). It is never
stored. Sentry is `SENTRY_URL` (or `AMPUP_MEMORY_SENTRY_URL`, or `url` in `~/.sentryclirc`), else
`https://sentry.io`: set it for a self-hosted Sentry, or to `https://de.sentry.io` for an organization in the EU.
Each issue is written as it stands (source `sentry:ORG/PROJECT/issues/ID`, an `incident` episode in the hosted
connector's format: status, counts, releases, error, stack and activity), and its new version retires the one before.
The first read lists the issues with events since `--since`; later reads start an hour before the newest event read,
and once a day 14 days back, for an issue resolved, ignored or assigned with no event since. An issue is written
again when its state changes (status, assignee, comments, title, culprit, level), or, for new events alone, at most
once a day, so a busy issue isn't written every round. A server a session starts by itself gets
`SENTRY_AUTH_TOKEN` and `SENTRY_URL` from that session's environment; the background service has neither, but it
carries `AMPUP_MEMORY_SENTRY_TOKEN_FILE` and `AMPUP_MEMORY_SENTRY_URL` and reads `~/.sentryclirc`.

### Any Iceberg REST catalog over S3

This is the configuration the repository's catalog tests run (Apache Iceberg's REST fixture on MinIO,
`tests/fixtures/rest-catalog/`):

```bash
docker compose -f tests/fixtures/rest-catalog/docker-compose.yml up -d --wait

AMPUP_MEMORY_PLATFORM=rest \
AMPUP_MEMORY_REST_URI=http://localhost:18181 \
AMPUP_MEMORY_WAREHOUSE=s3://memstore/warehouse \
AMPUP_MEMORY_REST_PROPS='s3.endpoint=http://localhost:19000;s3.access-key-id=admin;s3.secret-access-key=password;s3.region=us-east-1;s3.path-style-access=true' \
AMPUP_MEMORY_INDEX_URI=s3://memstore/index \
AWS_ACCESS_KEY_ID=admin AWS_SECRET_ACCESS_KEY=password AWS_REGION=us-east-1 \
AWS_ENDPOINT=http://localhost:19000 AWS_ALLOW_HTTP=true \
ampup-memory serve org 8080
```

`AMPUP_MEMORY_REST_PROPS` takes `key=value;key=value` catalog and storage properties (a REST catalog's `credential`
and `scope` for OAuth, `s3.*` for storage). The search index reaches S3 through the `AWS_*` variables.

Results on REST catalogs (from the catalog tests):

- Apache Iceberg's REST fixture 1.10 (JDBC catalog, S3FileIO) on MinIO: every flow passes, including forget and
  org delete leaving no object of the removed memory in the bucket.
- Apache Polaris 1.8 as shipped refuses two things a table rewrite needs (`ALLOW_TABLE_LOCATION_OVERLAP`,
  `DROP_WITH_PURGE_ENABLED`). With both flags on, every flow passes. Without them, `users forget` and `org delete`
  refuse up front and change nothing.

### GCS, S3, Azure

Point the catalog's warehouse at a bucket (`gs://…`, `s3://…`, `abfss://<filesystem>@<account>.dfs.core.windows.net/…`)
and `AMPUP_MEMORY_INDEX_URI` at a durable location. GCS is reached with `GCP_ACCESS_TOKEN` when set, else application
default credentials; S3 with the standard `AWS_*` credentials and `AWS_ENDPOINT`, `AWS_REGION`.

Setup steps for a specific cloud (Terraform, Cloud Run, Kubernetes manifests) are not part of this guide yet.

## Model providers

Extraction, compiling, advice, judging, routing and dreaming all call a model through one backend
(`AMPUP_MEMORY_EXTRACT`, with per-job overrides). OpenRouter is the default; Amazon Bedrock, Vertex AI, Snowflake
Cortex, Anthropic, and any OpenAI-compatible API (OpenAI, vLLM, Ollama) are direct options for requests that must
stay in a cloud or on a machine. See [configuration.md](configuration.md#models).

```bash
# a local model through Ollama
AMPUP_MEMORY_EXTRACT=openai AMPUP_MEMORY_EXTRACT_URL=http://localhost:11434/v1 AMPUP_MEMORY_EXTRACT_MODEL=qwen3 ampup-memory serve --local
```

## Several servers on one catalog

Any number of servers may share a catalog.

- **Readiness.** `GET /ready` answers 503 until warm-up is done (the directory, the ledger, every scope with its
  indexes), then 200. Past `AMPUP_MEMORY_READY_TIMEOUT_SECS` (300) of slowness it answers 200 anyway and keeps warming.
  Any other unknown path (`/health`, `/healthz`) answers as soon as the port is bound.
- **One writer.** Background writing (extraction, compiling, advice learning, session sweeps, index updates) runs
  on one server per catalog at a time: the holder of a lease kept beside the system tables. A commit is refused
  once the lease is lost. `AMPUP_MEMORY_WRITER_LEASE=off` skips it, for a catalog no other server shares. Alert on
  `sum(memstore_writer_lease_held) == 0` for 5 minutes.
- **Freshness.** A server reads other servers' writes within `AMPUP_MEMORY_MAX_STALENESS_MS` (1000); a read blocks only
  on a cold cache, right after its own write, or past `AMPUP_MEMORY_HARD_STALENESS_MS` (60000).
- **Stopping.** On SIGTERM a server answers `/ready` with 503 but keeps answering for
  `AMPUP_MEMORY_SHUTDOWN_DRAIN_SECS` (5), then finishes in-flight requests for up to `AMPUP_MEMORY_SHUTDOWN_WAIT_SECS` (60).
  A Kubernetes `terminationGracePeriodSeconds` must cover both plus about 30 seconds.
- **Behind a load balancer.** With sign-in (`AMPUP_MEMORY_CALLER_IDENTITY=token`), set `AMPUP_MEMORY_PUBLIC_URL` to the
  URL people reach; a server listening beyond loopback won't start without it. Keep `AMPUP_MEMORY_API_KEYS` the same
  file on every server (each reads it again within 2 seconds of a change), and `AMPUP_MEMORY_HEADER_TIMEOUT_SECS`
  (660) above the balancer's idle timeout. The connection and in-flight caps are per server.
- **Throughput.** Every commit is one compare-and-set on the ledger table: about 1 to 2.5 commits a second under
  contention. Batch writes (`/ingest` takes 500 episodes a commit; `/runs` takes 1,000 runs) where throughput
  matters.

## Maintenance

- Every commit expires its table's snapshots past `AMPUP_MEMORY_MAINTAIN_KEEP` (10). `ampup-memory maintain <scope> [keep]`
  does it at once.
- Rewrites (table and ledger compaction, scheduled purges, dream passes) run only in a nightly window,
  `AMPUP_MEMORY_MAINTAIN_WINDOW` (default `02:00-06:00 America/Los_Angeles`; `always` or `off`), one scope at a time.
  Each scope's turn logs one `maintenance: {…}` line of counts.
- `ampup-memory compact-ledger [--dry-run] [--local DIR]` compacts the ledger at once; the window does it up to
  `AMPUP_MEMORY_LEDGER_COMPACT_MAX_FILES` (2000) data files and asks for the command past that
  (`memstore_ledger_compaction_cli_needed`).
- Deletion never waits for the window: forget and retract take effect on request.

## Backup and restore

Back up, together and at one point in time:

- the warehouse: every scope's namespace with its data, metadata and episode bodies;
- the `memory_system` namespace: `ledger` (without it nothing is visible), `directory`, `directory_snapshot`,
  `audit`, `key_usage`;
- the catalog's own record of tables (for a REST catalog, its database);
- outside the catalog: `AMPUP_MEMORY_API_KEYS`, the disabled-users file, and `AMPUP_MEMORY_SECRET_KEY`.

Not the search indexes: servers rebuild them (`ampup-memory index <scope>` at once). Take the backup with servers
stopped, or from a storage snapshot taken after the catalog's, since commits land in the tables first and the
ledger last. A backup holds what was deleted after it: apply forgets and deletes to backups too, or expire them.

## Observability

- **Metrics.** `/metrics` is Prometheus text. Serve it on its own port with `AMPUP_MEMORY_METRICS_PORT=9464` (expose it
  to the scraper only), or read it on the public port with the operator's admin key. Labels come from fixed sets:
  route templates, statuses, reason codes, the directory's orgs. Useful series: `memstore_http_requests_total`,
  `memstore_http_request_seconds`, `memstore_auth_refusals_total`, `memstore_index_freshness_lag_seconds`,
  `memstore_store_commit_failures_total`, `memstore_extraction_backlog_episodes`, `memstore_model_tokens_total`,
  `memstore_model_cache_total`, `memstore_writer_lease_held`, `memstore_directory_blocked`.
- **Slow requests.** A request taking `AMPUP_MEMORY_SLOW_REQUEST_MS` (1000) or longer is logged with its route and time.
  Every advice call and every MCP request is logged once with the caller's app and outcome.
- **Errors to Sentry.** Set `SENTRY_DSN` (and `SENTRY_ENVIRONMENT` other than `local`) to report 500s and panics.
  No DSN is compiled in.
- `ampup-memory support-bundle --engine <url> [--admin-key-file F] [--logs-file F]` writes a redacted diagnostics
  bundle to a directory and sends nothing.

## Caps

- `AMPUP_MEMORY_MAX_EPISODES` caps the episodes a deployment holds across every org; a write past it answers 403
  `{"cap_reached": true}`. Reads go on.
- Workspaces take `--max-episodes`, `--max-facts` and `--max-index-bytes`.

## Telemetry and the control plane

The engine can enroll with a control plane (`ampup-memory deployment enroll`) and then send counts-only telemetry. It
sends nothing unless enrolled; `AMPUP_MEMORY_TELEMETRY=off` sends and writes nothing. `ampup-memory telemetry schema` prints
the contract, and `GET /telemetry` (admin key) shows exactly what would be sent. The hosted control plane and
its onboarding wizard are not part of this repo, and the `ampup-memory agent` deployment agent is not documented here.

## Testing

```bash
cargo test                                  # local in-memory catalog, servers inside the test process
cargo test --test it <file>::               # one file's flows (tests/it/ is one test binary)
```

The REST-catalog flows in `tests/it/rest_catalog.rs` run against the fixture above with
`source tests/fixtures/rest-catalog/env.sh`; without `AMPUP_MEMORY_TEST_REST_URI` they skip.
