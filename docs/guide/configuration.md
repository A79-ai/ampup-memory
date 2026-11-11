# Configuration

Everything is configured from the environment. Defaults are in parentheses. Per-org and per-team settings, set
with `memstore org settings <org> [--team <team>] --<setting> <value>` or `/admin {"op": "set_settings"}`, win
over the server's variables where both exist; a team's setting wins over its org's.

## Server

| Variable | What it does |
|---|---|
| `MEMSTORE_PLATFORM` | The catalog: `local`, `rest`, `gcp`, `snowflake`, `databricks` ([self-hosting.md](self-hosting.md)). Not needed with `serve --local`. |
| `MEMSTORE_WAREHOUSE`, `MEMSTORE_REST_URI`, `MEMSTORE_REST_PROPS` | The `rest` (and `local`) catalog. |
| `MEMSTORE_INDEX_URI` | Where search indexes live: a path, `gs://…`, `s3://…` (`~/.cache/memstore/index`). |
| `MEMSTORE_SERVE_BIND` | The address to listen on (127.0.0.1). `serve --local` refuses anything else. |
| `MEMSTORE_PUBLIC_URL` | The server's public URL (like `https://memory.example.com`), named in sign-in discovery and every 401's `WWW-Authenticate`; never taken from a request's `Host`. With `token` identity a server that listens beyond loopback refuses to start without it. |
| `MEMSTORE_CALLER_IDENTITY` | How callers are identified: unset, `token`, `google`, `snowflake`, or a list ([security-model.md](security-model.md)). |
| `MEMSTORE_METRICS_PORT` | Serve `/metrics` on its own port. |
| `MEMSTORE_MAX_STALENESS_MS` (1000), `MEMSTORE_HARD_STALENESS_MS` (60000) | How stale a cached read may be before a background refresh, and before a read blocks. |
| `MEMSTORE_CATALOG_REFRESH_SECS` (300) | How often the catalog connection is rebuilt for fresh tokens. |
| `MEMSTORE_STORAGE_REQUESTS` (64) | Object-store requests in flight per process. |
| `MEMSTORE_READY_TIMEOUT_SECS` (300) | How long `/ready` waits for warm-up before answering 200 anyway. |
| `MEMSTORE_WRITER_LEASE` | `off` skips the writer lease (one server per catalog only). |
| `MEMSTORE_SHUTDOWN_DRAIN_SECS` (5), `MEMSTORE_SHUTDOWN_WAIT_SECS` (60) | Drain and wait on SIGTERM. |
| `MEMSTORE_SLOW_REQUEST_MS` (1000) | Log requests at least this slow. |
| `MEMSTORE_MAX_EPISODES` | Cap the episodes a deployment holds. |
| `MEMSTORE_RATE_LIMIT`, `MEMSTORE_INGEST_RATE_LIMIT` | Per-caller token buckets, e.g. `60/min:120`; `off` for none. A batch costs a token per item. |
| `MEMSTORE_MAX_CONNECTIONS` (4096), `MEMSTORE_MAX_IN_FLIGHT` (1024) | Open connections and requests being answered at once, per port; past either a request is answered 503 with `Retry-After: 1`. `off` lifts a cap. |
| `MEMSTORE_MAX_IN_FLIGHT_PER_CALLER` (64) | One caller's requests under way at once; past it 429. `off` lifts it. |
| `MEMSTORE_HEADER_TIMEOUT_SECS` (660) | How long a connection may take to send a request's headers, idle keep-alive ones too. Keep it above the load balancer's idle timeout. |
| `MEMSTORE_SERVE_WORKSPACES=1` | Serve every workspace side by side (as `serve --workspaces`). |

## Models

| Variable | What it does |
|---|---|
| `OPENROUTER_API_KEY` | The default backend's key. With no model key at all, nothing is extracted, compiled or learned. |
| `MEMSTORE_EXTRACT` | The backend: `openrouter` (default with a key), `openrouter-batch`, `openai`, `anthropic`, `bedrock`, `vertex`, `cortex`, or `off`. |
| `MEMSTORE_EXTRACT_MODEL`, `MEMSTORE_EXTRACT_URL`, `MEMSTORE_EXTRACT_KEY` (or `_KEY_FILE`), `MEMSTORE_EXTRACT_REGION` | The extraction model, endpoint, key and region. |
| `MEMSTORE_COMPILE`, `MEMSTORE_COMPILE_MODEL`, `MEMSTORE_AIP_MODEL` | Compiling skills and AIP graphs (falls back to extraction's backend). `MEMSTORE_COMPILE=off` stops compiling. |
| `MEMSTORE_ADVICE`, `MEMSTORE_ADVICE_MODEL` | Learning advice. `MEMSTORE_ADVICE=off` stops learning. |
| `MEMSTORE_ADVICE_ROUTE`, `MEMSTORE_ADVICE_ROUTE_MODEL` | Routing `get_advice`. `MEMSTORE_ADVICE_ROUTE=off` turns `get_advice` off. |
| `MEMSTORE_JUDGE`, `MEMSTORE_JUDGE_MODEL` | Judging sessions and runs. `MEMSTORE_JUDGE=off` judges nothing. |
| `MEMSTORE_DREAM`, `MEMSTORE_AGGREGATE_MODEL`, `MEMSTORE_PROFILE_MODEL` | Dreaming, summaries, profiles. |
| `MEMSTORE_DECIDE` | A decisions model (used for step advice relevance and public-capability suggestions). |
| `MEMSTORE_MODEL_CACHE` | Where model replies are cached: a directory, `gs://`, `s3://`, `file://`, or `off`. |

Backends and their defaults:

| `MEMSTORE_EXTRACT` | API | Default model | Key |
|---|---|---|---|
| `openrouter` | OpenRouter; your own provider keys pass through where the account holds them | `anthropic/claude-haiku-5.5` (learning and compiling: `anthropic/claude-opus-5.5`) | `OPENROUTER_API_KEY` |
| `openrouter-batch` | OpenRouter's Batch API: half the price, answers in minutes | as above | `OPENROUTER_API_KEY` |
| `openai` | OpenAI-compatible chat (OpenAI, vLLM, Ollama) | `gpt-5-mini` | `OPENAI_API_KEY` |
| `anthropic` | Anthropic Messages | `claude-haiku-5-5` (learning and compiling: `claude-opus-5-5`) | `ANTHROPIC_API_KEY` |
| `bedrock` | Amazon Bedrock Converse, in `MEMSTORE_EXTRACT_REGION` or `AWS_REGION` (`us-east-1`) | `us.anthropic.claude-haiku-4-5-20251001-v1:0` | `AWS_BEARER_TOKEN_BEDROCK` |
| `vertex` | Vertex AI's OpenAI-compatible endpoint in `GCP_PROJECT` | `google/gemini-2.5-flash` | Google application default credentials |
| `cortex` | Snowflake Cortex on `SNOWFLAKE_ACCOUNT` | `claude-sonnet-4-5` | `SNOWFLAKE_PAT` |

Requests carry `X-Title: memstore`, so their spend shows apart on a shared key.

## Extraction

| Variable | What it does |
|---|---|
| `MEMSTORE_EXTRACT_GOAL_BLIND=1` | Also extract episodes the schema's `extract` section doesn't name, on every scope. |
| `MEMSTORE_EXTRACT_SECS` (5) | How soon after landing an episode is read. |
| `MEMSTORE_EXTRACT_SCOPES` (4) | Scopes extracted at once. |
| `MEMSTORE_EXTRACT_PARALLEL` (8) | Requests in flight. |
| `MEMSTORE_EXTRACT_THINKING=off` | Claude models answer without thinking first, on every call through the Anthropic API; those that always think (Opus 5.5 and later, Fable, Mythos) are asked for low effort, and Sonnet 5.5 for `between_tools`. |
| `MEMSTORE_EXTRACT_PRICE` | `<$/M in>,<$/M out>`, for `backfill --max-usd` and estimates. |
| `MEMSTORE_BATCH_GATHER_SECS` (5), `MEMSTORE_BATCH_MAX_REQUESTS` (5000), `MEMSTORE_BATCH_MAX_MB` (100) | Batch API gathering. |

## Embeddings and search

| Variable | What it does |
|---|---|
| `MEMSTORE_EMBED` | `local`, `openai` or `openrouter` (chosen from the keys present when unset). |
| `MEMSTORE_EMBED_MODEL`, `MEMSTORE_EMBED_DIMS`, `MEMSTORE_EMBED_URL`, `MEMSTORE_EMBED_KEY` (or `_KEY_FILE`) | The embedding model and API. |
| `MEMSTORE_MODEL_DIR` | Where local embedding models are kept (`~/.cache/memstore/models`). |
| `MEMSTORE_EMBED_CACHE` | Where computed vectors are cached, or `off`. |
| `MEMSTORE_CHUNK_BYTES` (1500), `MEMSTORE_CHUNK_OVERLAP` (150) | Passage chunking. |
| `MEMSTORE_VIEW_FACTS` (200000) | Values and entities typed queries keep in memory; 0 keeps none. |
| `MEMSTORE_FRESHNESS=1`, `MEMSTORE_FRESHNESS_MARKS` | Mark stale and restated values at read time. |

## Compiling

| Variable | What it does |
|---|---|
| `MEMSTORE_COMPILE_SOURCES` | Source prefixes the server's own scope compiles from (comma-separated, or `*`). |
| `MEMSTORE_COMPILE_SECS` (60) | How often `serve` compiles. |
| `MEMSTORE_COMPILE_BATCH` (12), `MEMSTORE_COMPILE_BATCH_CHARS`, `MEMSTORE_COMPILE_MAX_TOKENS` (16000), `MEMSTORE_COMPILE_PARALLEL` (4) | Round size and parallel graphs. |

## Advice and sessions

| Variable | What it does |
|---|---|
| `MEMSTORE_ADVICE_SECS` (600) | How often `serve` learns. |
| `MEMSTORE_ADVICE_MIN_NEW` (10) | New rewarded runs before a learn; also the runs a person needs for personal notes. |
| `MEMSTORE_ADVICE_NOTE_MIN_NEW` (5) | New runs before a note is rewritten. |
| `MEMSTORE_ADVICE_LEARN=off` | Stop the learning timer. |
| `MEMSTORE_ADVICE_LEARN_PEOPLE` (3, never below 2) | People a team note's runs must come from. |
| `MEMSTORE_ADVICE_EVIDENCE` (3) | People a note must have helped before it is served outside the trial arm. 0 serves every note. |
| `MEMSTORE_ADVICE_TRIAL` (0.2), `MEMSTORE_ADVICE_HOLDOUT` (0) | Session arms. |
| `MEMSTORE_ADVICE_PERSONAL_MAX` (3) | People whose personal notes are learned per round; 0 turns personal notes off. |
| `MEMSTORE_ADVICE_JUDGE=off`, `MEMSTORE_ADVICE_JUDGE_AFTER_SECS` (1800), `MEMSTORE_ADVICE_JUDGE_MAX` (50) | Judging unrewarded runs. |
| `MEMSTORE_ADVICE_FORMAT` | `notes` (default) or `checklist`. |
| `MEMSTORE_ADVICE_ROUTE_SHORTLIST` (60) | Lines above which routing shortlists first; 0 never. |
| `MEMSTORE_ADVICE_CURATION=on`, `MEMSTORE_ADVICE_RETIRE_MIN` (8), `MEMSTORE_ADVICE_RETIRE_MARGIN` (0.2), `MEMSTORE_ADVICE_RETIRE_JUDGED` | Curation. |
| `MEMSTORE_ADVICE_ASK_WAIT_MS` | How long an agent's `get_advice` waits before "still being looked up". |
| `MEMSTORE_PUBLIC_CAPABILITIES=off` | No public skill or MCP server suggestions. |
| `MEMSTORE_SESSION_IDLE_SECS` (1800), `MEMSTORE_SESSION_SWEEP_SECS` (60) | Ending idle sessions; judging ended ones. |
| `MEMSTORE_RUNS_PER_DAY` (100), `MEMSTORE_LOG_PER_DAY` (1000), `MEMSTORE_SESSIONS_PER_DAY` (1000) | Per-person daily caps. |

## Memory of what users say

| Variable | What it does |
|---|---|
| `MEMSTORE_KEEP=off` | Don't keep preferences and decisions from session text, and don't share them. |
| `MEMSTORE_PROFILE=1` | Fold each person's notes into a profile. |
| `MEMSTORE_TRANSCRIPTS=1` | Keep each coding-agent session's whole transcript, for its person alone. |
| `MEMSTORE_AGGREGATE_SECS` (3600) | The hourly pass for summaries and team sharing. |
| `MEMSTORE_MCP_TOOLS` | `all`, or a comma-separated list of tools to list over MCP. |

## Maintenance

| Variable | What it does |
|---|---|
| `MEMSTORE_MAINTAIN_WINDOW` (`02:00-06:00 America/Los_Angeles`) | When rewrites run; `always` or `off`. |
| `MEMSTORE_MAINTAIN_KEEP` (10), `MEMSTORE_MAINTAIN_SECS` (600), `MEMSTORE_MAINTAIN_GRACE_SECS` (120) | Snapshot expiry. |
| `MEMSTORE_MAINTAIN_PACE` (`2s`), `MEMSTORE_MAINTAIN_BUDGET` | Pace and budget of the window. |
| `MEMSTORE_LEDGER_COMPACT=off`, `MEMSTORE_LEDGER_COMPACT_FILES` (500), `MEMSTORE_LEDGER_COMPACT_EVERY` (`24h`), `MEMSTORE_LEDGER_COMPACT_MAX_FILES` (2000) | Ledger compaction. |
| `MEMSTORE_COMPACT=off`, `MEMSTORE_COMPACT_FILES` (200), `MEMSTORE_COMPACT_MAX_FILES` (20000), `MEMSTORE_COMPACT_MAX_BYTES` (1 GiB) | Table compaction. |
| `MEMSTORE_PURGE_GRACE` (`7d`) | How long a deleted or aged-out episode stays hidden before a scheduled purge. |
| `MEMSTORE_DREAM_ON`, `MEMSTORE_DREAM_SECS`, `MEMSTORE_DREAM_FACTS` (200), `MEMSTORE_DREAM_CALLS` (100), `MEMSTORE_DREAM_PARALLEL` (8), `MEMSTORE_DREAM_BUDGET` (`10m`) | Dreaming. |

## Identity

| Variable | What it does |
|---|---|
| `MEMSTORE_API_KEYS` | A keys file (deprecated for everything but admin keys; use `memstore apikey`). Read again within 2 seconds when it changes; keep it the same on every server. |
| `MEMSTORE_JWT_JWKS_URL`, `MEMSTORE_JWT_PUBLIC_KEY`, `MEMSTORE_JWT_SECRET`, `MEMSTORE_JWT_ISSUER`, `MEMSTORE_JWT_AUDIENCE`, `MEMSTORE_JWT_ORG_CLAIM`, `MEMSTORE_JWT_GROUPS_CLAIM`, `MEMSTORE_JWT_USER_CLAIMS` | JWT callers. |
| `MEMSTORE_OIDC_ISSUER`, `MEMSTORE_OIDC_AUDIENCE`, `MEMSTORE_OIDC_CLI_CLIENT_ID`, `MEMSTORE_OIDC_CLI_CLIENT_SECRET`, `MEMSTORE_OIDC_CLI_CALLBACK_PORT` | People's sign-in through the server's identity provider. Serve a client secret only for a public client. |
| `MEMSTORE_IAP_AUDIENCE` | People through Google's Identity-Aware Proxy. |
| `MEMSTORE_SECRET_KEY` | 32 bytes (base64 or hex) that seal identity-provider secrets and directory snapshots. Same on every server. Without it every snapshot is checked against the directory log on load. |
| `MEMSTORE_DIRECTORY=required` | Every caller must select an org. |
| `MEMSTORE_DISABLED_FILE`, `MEMSTORE_TEAM_CSV` | A server's own disabled-users list and team csv. |
| `MEMSTORE_ALLOW_LOCAL_ISSUERS=1` | Allow an identity provider on this machine (development). |
| `MEMSTORE_AUDIT_FLUSH_SECS` (60) | How often the audit buffer is written. |

## Data handling

| Variable | What it does |
|---|---|
| `MEMSTORE_TRAIN_LOG` | Keep every model call whole, per org the operator turns on, to train smaller models (`memstore train-log export`). Off unless named. |
| `MEMSTORE_TELEMETRY=off` | Send and write no telemetry. |
| `SENTRY_DSN`, `SENTRY_ENVIRONMENT` | Error reports. |

## Client and hooks

| Variable | What it does |
|---|---|
| `MEMSTORE_URL`, `MEMSTORE_ORG`, `MEMSTORE_TOKEN`, `MEMSTORE_SCOPE` | Where the hooks and `memstore mcp` send, and as whom. |
| `MEMSTORE_CAPTURE=off`, `MEMSTORE_RECALL=every`, `MEMSTORE_STARTER=off` | What the hooks send. |
| `MEMSTORE_CONFIG_DIR` | Where contexts and sign-ins are kept (`~/.memstore`). |
| `MEMSTORE_CONTEXT` | The context for one command (as `--context`). |
| `MEMSTORE_LOGIN_PORT` | The loopback port for browser sign-in. |
