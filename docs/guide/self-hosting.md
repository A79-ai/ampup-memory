# Self-hosting

AmpUp Memory stores everything in Apache Iceberg tables in a catalog you choose, over object storage you own. The
server is one binary, `memstore serve`. Search indexes are Lance datasets derived from the tables.

## Catalogs

`MEMSTORE_PLATFORM` picks the catalog. Every non-local platform speaks the Iceberg REST catalog protocol; an adapter
only supplies the address, auth and storage properties.

| `MEMSTORE_PLATFORM` | Catalog | Settings |
|---|---|---|
| (`serve --local`) | a JSON catalog file under one directory | `--local [DIR]`, default `~/.memstore` |
| `local` | in-memory catalog over a local warehouse | `MEMSTORE_WAREHOUSE` |
| `rest` | any Iceberg REST catalog (Apache Polaris, the Iceberg REST fixture, …) | `MEMSTORE_REST_URI`, `MEMSTORE_WAREHOUSE`, `MEMSTORE_REST_PROPS` |
| `gcp` | BigLake metastore Iceberg REST over Cloud Storage | `GCP_PROJECT`, `GCS_BUCKET` |
| `snowflake` | Snowflake Horizon Iceberg REST, vended credentials | `SNOWFLAKE_ACCOUNT`, `SNOWFLAKE_DATABASE`, `SNOWFLAKE_ROLE`, `SNOWFLAKE_PAT` |
| `databricks` | Databricks Unity Catalog Iceberg REST | `DATABRICKS_HOST`, `DATABRICKS_CATALOG`, `DATABRICKS_TOKEN` or `DATABRICKS_CLIENT_ID` + `DATABRICKS_CLIENT_SECRET` |

Storage is resolved per table location: `gs://`, `s3://`, `abfss://` (Azure Data Lake Storage Gen2) and `file://`.
A secret variable can also be given as a file: `<VAR>_FILE` (for mounted secrets) where the code reads it that way
(`SNOWFLAKE_PAT`, the `DATABRICKS_*` credentials).

On BigQuery or Snowflake, the same tables are readable by their own engines.

### Laptop: `serve --local`

```bash
memstore serve --local                 # ~/.memstore, http://127.0.0.1:8080
memstore serve --local ./store org 8090   # another directory, scope and port
```

- `DIR/warehouse` holds the tables, `DIR/index` the search index, `DIR/embeddings` the embedding cache, and
  `DIR/catalog.json` names each table's current metadata file, so a restart picks up where it stopped.
- No Docker, no catalog service, no sign-in. Every request reads as one caller in group `local` (or the groups
  given after the port). It binds to 127.0.0.1 and refuses to start if `MEMSTORE_SERVE_BIND` names anything else;
  requests whose `Host` or `Origin` isn't this machine are refused.
- One process owns a directory at a time. Commands with `--local DIR` (`schema set`, `runs load`, `compile`,
  `advice`, `extract`, `load`, …) need the server stopped.

### Any Iceberg REST catalog over S3

This is the configuration the repository's catalog tests run (Apache Iceberg's REST fixture on MinIO,
`tests/fixtures/rest-catalog/`):

```bash
docker compose -f tests/fixtures/rest-catalog/docker-compose.yml up -d --wait

MEMSTORE_PLATFORM=rest \
MEMSTORE_REST_URI=http://localhost:18181 \
MEMSTORE_WAREHOUSE=s3://memstore/warehouse \
MEMSTORE_REST_PROPS='s3.endpoint=http://localhost:19000;s3.access-key-id=admin;s3.secret-access-key=password;s3.region=us-east-1;s3.path-style-access=true' \
MEMSTORE_INDEX_URI=s3://memstore/index \
AWS_ACCESS_KEY_ID=admin AWS_SECRET_ACCESS_KEY=password AWS_REGION=us-east-1 \
AWS_ENDPOINT=http://localhost:19000 AWS_ALLOW_HTTP=true \
memstore serve org 8080
```

`MEMSTORE_REST_PROPS` takes `key=value;key=value` catalog and storage properties (a REST catalog's `credential`
and `scope` for OAuth, `s3.*` for storage). The search index reaches S3 through the `AWS_*` variables.

Results on REST catalogs (from the catalog tests):

- Apache Iceberg's REST fixture 1.10 (JDBC catalog, S3FileIO) on MinIO: every flow passes, including forget and
  org delete leaving no object of the removed memory in the bucket.
- Apache Polaris 1.8 as shipped refuses two things a table rewrite needs (`ALLOW_TABLE_LOCATION_OVERLAP`,
  `DROP_WITH_PURGE_ENABLED`). With both flags on, every flow passes. Without them, `users forget` and `org delete`
  refuse up front and change nothing.

### GCS, S3, Azure

Point the catalog's warehouse at a bucket (`gs://…`, `s3://…`, `abfss://<filesystem>@<account>.dfs.core.windows.net/…`)
and `MEMSTORE_INDEX_URI` at a durable location. GCS is reached with `GCP_ACCESS_TOKEN` when set, else application
default credentials; S3 with the standard `AWS_*` credentials and `AWS_ENDPOINT`, `AWS_REGION`.

Setup steps for a specific cloud (Terraform, Cloud Run, Kubernetes manifests) are not part of this guide yet.

## Model providers

Extraction, compiling, advice, judging, routing and dreaming all call a model through one backend
(`MEMSTORE_EXTRACT`, with per-job overrides). OpenRouter is the default; Amazon Bedrock, Vertex AI, Snowflake
Cortex, Anthropic, and any OpenAI-compatible API (OpenAI, vLLM, Ollama) are direct options for requests that must
stay in a cloud or on a machine. See [configuration.md](configuration.md#models).

```bash
# a local model through Ollama
MEMSTORE_EXTRACT=openai MEMSTORE_EXTRACT_URL=http://localhost:11434/v1 MEMSTORE_EXTRACT_MODEL=qwen3 memstore serve --local
```

## Several servers on one catalog

Any number of servers may share a catalog.

- **Readiness.** `GET /ready` answers 503 until warm-up is done (the directory, the ledger, every scope with its
  indexes), then 200. Past `MEMSTORE_READY_TIMEOUT_SECS` (300) of slowness it answers 200 anyway and keeps warming.
  Any other unknown path (`/health`, `/healthz`) answers as soon as the port is bound.
- **One writer.** Background writing (extraction, compiling, advice learning, session sweeps, index updates) runs
  on one server per catalog at a time: the holder of a lease kept beside the system tables. A commit is refused
  once the lease is lost. `MEMSTORE_WRITER_LEASE=off` skips it, for a catalog no other server shares. Alert on
  `sum(memstore_writer_lease_held) == 0` for 5 minutes.
- **Freshness.** A server reads other servers' writes within `MEMSTORE_MAX_STALENESS_MS` (1000); a read blocks only
  on a cold cache, right after its own write, or past `MEMSTORE_HARD_STALENESS_MS` (60000).
- **Stopping.** On SIGTERM a server answers `/ready` with 503 but keeps answering for
  `MEMSTORE_SHUTDOWN_DRAIN_SECS` (5), then finishes in-flight requests for up to `MEMSTORE_SHUTDOWN_WAIT_SECS` (60).
  A Kubernetes `terminationGracePeriodSeconds` must cover both plus about 30 seconds.
- **Throughput.** Every commit is one compare-and-set on the ledger table: about 1 to 2.5 commits a second under
  contention. Batch writes (`/ingest` takes 500 episodes a commit; `/runs` takes 1,000 runs) where throughput
  matters.

## Maintenance

- Every commit expires its table's snapshots past `MEMSTORE_MAINTAIN_KEEP` (10). `memstore maintain <scope> [keep]`
  does it at once.
- Rewrites (table and ledger compaction, scheduled purges, dream passes) run only in a nightly window,
  `MEMSTORE_MAINTAIN_WINDOW` (default `02:00-06:00 America/Los_Angeles`; `always` or `off`), one scope at a time.
  Each scope's turn logs one `maintenance: {…}` line of counts.
- `memstore compact-ledger [--dry-run] [--local DIR]` compacts the ledger at once; the window does it up to
  `MEMSTORE_LEDGER_COMPACT_MAX_FILES` (2000) data files and asks for the command past that
  (`memstore_ledger_compaction_cli_needed`).
- Deletion never waits for the window: forget and retract take effect on request.

## Backup and restore

Back up, together and at one point in time:

- the warehouse: every scope's namespace with its data, metadata and episode bodies;
- the `memory_system` namespace: `ledger` (without it nothing is visible), `directory`, `directory_snapshot`,
  `audit`, `key_usage`;
- the catalog's own record of tables (for a REST catalog, its database);
- outside the catalog: `MEMSTORE_API_KEYS`, the disabled-users file, and `MEMSTORE_SECRET_KEY`.

Not the search indexes: servers rebuild them (`memstore index <scope>` at once). Take the backup with servers
stopped, or from a storage snapshot taken after the catalog's, since commits land in the tables first and the
ledger last. A backup holds what was deleted after it: apply forgets and deletes to backups too, or expire them.

## Observability

- **Metrics.** `/metrics` is Prometheus text. Serve it on its own port with `MEMSTORE_METRICS_PORT=9464` (expose it
  to the scraper only), or read it on the public port with the operator's admin key. Labels come from fixed sets:
  route templates, statuses, reason codes, the directory's orgs. Useful series: `memstore_http_requests_total`,
  `memstore_http_request_seconds`, `memstore_auth_refusals_total`, `memstore_index_freshness_lag_seconds`,
  `memstore_store_commit_failures_total`, `memstore_extraction_backlog_episodes`, `memstore_model_tokens_total`,
  `memstore_model_cache_total`, `memstore_writer_lease_held`, `memstore_directory_blocked`.
- **Slow requests.** A request taking `MEMSTORE_SLOW_REQUEST_MS` (1000) or longer is logged with its route and time.
  Every advice call and every MCP request is logged once with the caller's app and outcome.
- **Errors to Sentry.** Set `SENTRY_DSN` (and `SENTRY_ENVIRONMENT` other than `local`) to report 500s and panics.
  No DSN is compiled in.
- `memstore support-bundle --engine <url> [--admin-key-file F] [--logs-file F]` writes a redacted diagnostics
  bundle to a directory and sends nothing.

## Caps

- `MEMSTORE_MAX_EPISODES` caps the episodes a deployment holds across every org; a write past it answers 403
  `{"cap_reached": true}`. Reads go on.
- Workspaces take `--max-episodes`, `--max-facts` and `--max-index-bytes`.

## Telemetry and the control plane

The engine can enroll with a control plane (`memstore deployment enroll`) and then send counts-only telemetry. It
sends nothing unless enrolled; `MEMSTORE_TELEMETRY=off` sends and writes nothing. `memstore telemetry schema` prints
the contract, and `GET /telemetry` (admin key) shows exactly what would be sent. The hosted control plane and
its onboarding wizard are not part of this repo, and the `memstore agent` deployment agent is not documented here.

## Testing

```bash
cargo test                                  # local in-memory catalog, servers inside the test process
cargo test --test it <file>::               # one file's flows (tests/it/ is one test binary)
```

The REST-catalog flows in `tests/it/rest_catalog.rs` run against the fixture above with
`source tests/fixtures/rest-catalog/env.sh`; without `MEMSTORE_TEST_REST_URI` they skip.
