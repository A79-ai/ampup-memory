# Configuration

Everything is configured from the environment. Defaults are in parentheses. Per-org and per-team settings, set
with `ampup-memory org settings <org> [--team <team>] --<setting> <value>` or `/admin {"op": "set_settings"}`, win
over the server's variables where both exist; a team's setting wins over its org's.

## Server

| Variable | What it does |
|---|---|
| `AMPUP_MEMORY_PLATFORM` | The catalog: `local`, `rest`, `gcp`, `snowflake`, `databricks` ([self-hosting.md](self-hosting.md)). Not needed with `serve --local`. |
| `AMPUP_MEMORY_WAREHOUSE`, `AMPUP_MEMORY_REST_URI`, `AMPUP_MEMORY_REST_PROPS` | The `rest` (and `local`) catalog. |
| `AMPUP_MEMORY_INDEX_URI` | Where search indexes live: a path, `gs://…`, `s3://…` (`~/.cache/memstore/index`). |
| `AMPUP_MEMORY_SERVE_BIND` | The address to listen on (127.0.0.1). `serve --local` refuses anything else. |
| `AMPUP_MEMORY_PUBLIC_URL` | The server's public URL (like `https://memory.example.com`), named in sign-in discovery and every 401's `WWW-Authenticate`; never taken from a request's `Host`. With `token` identity a server that listens beyond loopback refuses to start without it. |
| `AMPUP_MEMORY_CALLER_IDENTITY` | How callers are identified: unset, `token`, `google`, `snowflake`, or a list ([security-model.md](security-model.md)). |
| `AMPUP_MEMORY_METRICS_PORT` | Serve `/metrics` on its own port. |
| `AMPUP_MEMORY_MAX_STALENESS_MS` (1000), `AMPUP_MEMORY_HARD_STALENESS_MS` (60000) | How stale a cached read may be before a background refresh, and before a read blocks. |
| `AMPUP_MEMORY_CATALOG_REFRESH_SECS` (300) | How often the catalog connection is rebuilt for fresh tokens. |
| `AMPUP_MEMORY_STORAGE_REQUESTS` (64) | Object-store requests in flight per process. |
| `AMPUP_MEMORY_READY_TIMEOUT_SECS` (300) | How long `/ready` waits for warm-up before answering 200 anyway. |
| `AMPUP_MEMORY_WRITER_LEASE` | `off` skips the writer lease (one server per catalog only). |
| `AMPUP_MEMORY_SHUTDOWN_DRAIN_SECS` (5), `AMPUP_MEMORY_SHUTDOWN_WAIT_SECS` (60) | Drain and wait on SIGTERM. |
| `AMPUP_MEMORY_SLOW_REQUEST_MS` (1000) | Log requests at least this slow. |
| `AMPUP_MEMORY_MAX_EPISODES` | Cap the episodes a deployment holds. |
| `AMPUP_MEMORY_RATE_LIMIT`, `AMPUP_MEMORY_INGEST_RATE_LIMIT` | Per-caller token buckets, e.g. `60/min:120`; `off` for none. A batch costs a token per item. |
| `AMPUP_MEMORY_MAX_CONNECTIONS` (4096), `AMPUP_MEMORY_MAX_IN_FLIGHT` (1024) | Open connections and requests being answered at once, per port; past either a request is answered 503 with `Retry-After: 1`. `off` lifts a cap. |
| `AMPUP_MEMORY_MAX_IN_FLIGHT_PER_CALLER` (64) | One caller's requests under way at once; past it 429. `off` lifts it. |
| `AMPUP_MEMORY_HEADER_TIMEOUT_SECS` (660) | How long a connection may take to send a request's headers, idle keep-alive ones too. Keep it above the load balancer's idle timeout. |
| `AMPUP_MEMORY_SERVE_WORKSPACES=1` | Serve every workspace side by side (as `serve --workspaces`). |

## Models

| Variable | What it does |
|---|---|
| `OPENROUTER_API_KEY` | The default backend's key. With no model key at all, nothing is extracted, compiled or learned. |
| `AMPUP_MEMORY_LLM` | One backend for every role below whose own variable names none: `ollama`, any `AMPUP_MEMORY_EXTRACT` value, or `off`. `serve --local` keeps a choice made at its first run in `llm.json` in its directory, under the variables. |
| `AMPUP_MEMORY_LLM_MODEL`, `AMPUP_MEMORY_LLM_URL`, `AMPUP_MEMORY_LLM_KEY` | One model (the strong roles' too), base URL and key for it. `AMPUP_MEMORY_LLM_URL` alone means `openai` at that URL. |
| `AMPUP_MEMORY_LLM_CONTEXT` (32768) | The context, in tokens, Ollama loads the model with. |
| `AMPUP_MEMORY_EXTRACT` | The backend: `openrouter` (default with a key), `openrouter-batch`, `openai`, `anthropic`, `bedrock`, `vertex`, `cortex`, or `off`. |
| `AMPUP_MEMORY_EXTRACT_MODEL`, `AMPUP_MEMORY_EXTRACT_URL`, `AMPUP_MEMORY_EXTRACT_KEY` (or `_KEY_FILE`), `AMPUP_MEMORY_EXTRACT_REGION` | The extraction model, endpoint, key and region. |
| `AMPUP_MEMORY_COMPILE`, `AMPUP_MEMORY_COMPILE_MODEL`, `AMPUP_MEMORY_AIP_MODEL` | Compiling skills and AIP graphs (falls back to extraction's backend). `AMPUP_MEMORY_COMPILE=off` stops compiling. |
| `AMPUP_MEMORY_ADVICE`, `AMPUP_MEMORY_ADVICE_MODEL` | Learning advice. `AMPUP_MEMORY_ADVICE=off` stops learning. |
| `AMPUP_MEMORY_ADVICE_ROUTE`, `AMPUP_MEMORY_ADVICE_ROUTE_MODEL` | Routing `get_advice`. `AMPUP_MEMORY_ADVICE_ROUTE=off` turns `get_advice` off. |
| `AMPUP_MEMORY_JUDGE`, `AMPUP_MEMORY_JUDGE_MODEL` | Judging sessions and runs. `AMPUP_MEMORY_JUDGE=off` judges nothing. |
| `AMPUP_MEMORY_DREAM`, `AMPUP_MEMORY_AGGREGATE_MODEL`, `AMPUP_MEMORY_PROFILE_MODEL` | Dreaming, summaries, profiles. |
| `AMPUP_MEMORY_DECIDE` | A decisions model (used for step advice relevance and public-capability suggestions). |
| `AMPUP_MEMORY_MODEL_CACHE` | Where model replies are cached: a directory, `gs://`, `s3://`, `file://`, or `off`. |

Backends and their defaults:

| `AMPUP_MEMORY_EXTRACT` | API | Default model | Key |
|---|---|---|---|
| `openrouter` | OpenRouter; your own provider keys pass through where the account holds them | `anthropic/claude-haiku-5.5` (learning and compiling: `anthropic/claude-opus-5.5`) | `OPENROUTER_API_KEY` |
| `openrouter-batch` | OpenRouter's Batch API: half the price, answers in minutes | as above | `OPENROUTER_API_KEY` |
| `openai` | OpenAI-compatible chat (OpenAI, vLLM, LM Studio) | `gpt-5-mini` | `OPENAI_API_KEY` |
| `ollama` | Ollama's own chat API at `http://localhost:11434`, with the context size each request needs | `qwen2.5:3b` (every role) | none |
| `anthropic` | Anthropic Messages | `claude-haiku-5-5` (learning and compiling: `claude-opus-5-5`) | `ANTHROPIC_API_KEY` |
| `bedrock` | Amazon Bedrock Converse, in `AMPUP_MEMORY_EXTRACT_REGION` or `AWS_REGION` (`us-east-1`) | `us.anthropic.claude-haiku-4-5-20251001-v1:0` | `AWS_BEARER_TOKEN_BEDROCK` |
| `vertex` | Vertex AI's OpenAI-compatible endpoint in `GCP_PROJECT` | `google/gemini-2.5-flash` | Google application default credentials |
| `cortex` | Snowflake Cortex on `SNOWFLAKE_ACCOUNT` | `claude-sonnet-4-5` | `SNOWFLAKE_PAT` |

Requests carry `X-Title: ampup-memory`, so their spend shows apart on a shared key.

## Extraction

| Variable | What it does |
|---|---|
| `AMPUP_MEMORY_EXTRACT_GOAL_BLIND=1` | Also extract episodes the schema's `extract` section doesn't name, on every scope. |
| `AMPUP_MEMORY_EXTRACT_SECS` (5) | How soon after landing an episode is read. |
| `AMPUP_MEMORY_EXTRACT_SCOPES` (4) | Scopes extracted at once. |
| `AMPUP_MEMORY_EXTRACT_PARALLEL` (8) | Requests in flight. |
| `AMPUP_MEMORY_EXTRACT_THINKING=off` | Claude models answer without thinking first, on every call through the Anthropic API, OpenRouter or Bedrock; those that always think (Opus 5.5 and later, Fable, Mythos) are asked for low effort, and Sonnet 5.5 for `between_tools` (low effort on OpenRouter). |
| `AMPUP_MEMORY_EXTRACT_PRICE` | `<$/M in>,<$/M out>`, for `backfill --max-usd` and estimates. |
| `AMPUP_MEMORY_BATCH_GATHER_SECS` (5), `AMPUP_MEMORY_BATCH_MAX_REQUESTS` (5000), `AMPUP_MEMORY_BATCH_MAX_MB` (100) | Batch API gathering. |

## Embeddings and search

| Variable | What it does |
|---|---|
| `AMPUP_MEMORY_EMBED` | `local`, `openai` or `openrouter` (chosen from the keys present when unset). |
| `AMPUP_MEMORY_EMBED_MODEL`, `AMPUP_MEMORY_EMBED_DIMS`, `AMPUP_MEMORY_EMBED_URL`, `AMPUP_MEMORY_EMBED_KEY` (or `_KEY_FILE`) | The embedding model and API. |
| `AMPUP_MEMORY_MODEL_DIR` | Where local embedding models are kept (`~/.cache/memstore/models`). `ampup-memory embed warmup` fetches the model there ahead of time. |
| `HF_ENDPOINT`, `HF_HOME`, `HF_HUB_OFFLINE=1` | Where the local model is fetched from (a mirror), kept instead (over the above), or never fetched. Without it, search ranks by keyword alone. |
| `AMPUP_MEMORY_EMBED_CACHE` | Where computed vectors are cached, or `off`. |
| `AMPUP_MEMORY_CHUNK_BYTES` (1500), `AMPUP_MEMORY_CHUNK_OVERLAP` (150) | Passage chunking. |
| `AMPUP_MEMORY_VIEW_FACTS` (200000) | Values and entities typed queries keep in memory; 0 keeps none. |
| `AMPUP_MEMORY_FRESHNESS=1`, `AMPUP_MEMORY_FRESHNESS_MARKS` | Mark stale and restated values at read time. |

## Compiling

| Variable | What it does |
|---|---|
| `AMPUP_MEMORY_COMPILE_SOURCES` | Source prefixes the server's own scope compiles from (comma-separated, or `*`). |
| `AMPUP_MEMORY_COMPILE_SECS` (60) | How often `serve` compiles. |
| `AMPUP_MEMORY_COMPILE_BATCH` (12), `AMPUP_MEMORY_COMPILE_BATCH_CHARS`, `AMPUP_MEMORY_COMPILE_MAX_TOKENS` (16000), `AMPUP_MEMORY_COMPILE_PARALLEL` (4) | Round size and parallel graphs. |

## Advice and sessions

| Variable | What it does |
|---|---|
| `AMPUP_MEMORY_ADVICE_SECS` (600) | How often `serve` learns. |
| `AMPUP_MEMORY_ADVICE_MIN_NEW` (10) | New rewarded runs before a learn; also the runs a person needs for personal notes. |
| `AMPUP_MEMORY_ADVICE_NOTE_MIN_NEW` (5) | New runs before a note is rewritten. |
| `AMPUP_MEMORY_ADVICE_LEARN=off` | Stop the learning timer. |
| `AMPUP_MEMORY_ADVICE_LEARN_PEOPLE` (3, never below 2) | People a team note's runs must come from. |
| `AMPUP_MEMORY_ADVICE_EVIDENCE` (3) | People a note must have helped before it is reported ratified. Reported only: every note is served. |
| `AMPUP_MEMORY_ADVICE_HOLDOUT` (0) | Share of sessions routed but served no advice, to measure lift. |
| `AMPUP_MEMORY_ADVICE_PERSONAL_MAX` (3) | People whose personal notes are learned per round; 0 turns personal notes off. |
| `AMPUP_MEMORY_ADVICE_JUDGE=off`, `AMPUP_MEMORY_ADVICE_JUDGE_AFTER_SECS` (1800), `AMPUP_MEMORY_ADVICE_JUDGE_MAX` (50) | Judging unrewarded runs. |
| `AMPUP_MEMORY_ADVICE_FORMAT` | `notes` (default) or `checklist`. |
| `AMPUP_MEMORY_ADVICE_ROUTE_SHORTLIST` (60) | Lines above which routing shortlists first; 0 never. |
| `AMPUP_MEMORY_ADVICE_CURATION=on`, `AMPUP_MEMORY_ADVICE_RETIRE_MIN` (8), `AMPUP_MEMORY_ADVICE_RETIRE_MARGIN` (0.2), `AMPUP_MEMORY_ADVICE_RETIRE_JUDGED` | Curation. |
| `AMPUP_MEMORY_ADVICE_ASK_WAIT_MS` | How long an agent's `get_advice` waits before "still being looked up". |
| `AMPUP_MEMORY_PUBLIC_CAPABILITIES=off` | No public skill or MCP server suggestions. |
| `AMPUP_MEMORY_SESSION_IDLE_SECS` (1800), `AMPUP_MEMORY_SESSION_SWEEP_SECS` (60) | Ending idle sessions; judging ended ones. |
| `AMPUP_MEMORY_RUNS_PER_DAY` (100), `AMPUP_MEMORY_LOG_PER_DAY` (1000), `AMPUP_MEMORY_SESSIONS_PER_DAY` (1000) | Per-person daily caps. |

## Memory of what users say

| Variable | What it does |
|---|---|
| `AMPUP_MEMORY_KEEP=off` | Don't keep preferences and decisions from session text, and don't share them. |
| `AMPUP_MEMORY_PROFILE=1` | Fold each person's notes into a profile. |
| `AMPUP_MEMORY_TRANSCRIPTS=1` | Keep each coding-agent session's whole transcript, for its person alone. |
| `AMPUP_MEMORY_AGGREGATE_SECS` (3600) | The hourly pass for summaries and team sharing. |
| `AMPUP_MEMORY_MCP_TOOLS` | `all`, or a comma-separated list of tools to list over MCP. |

## Maintenance

| Variable | What it does |
|---|---|
| `AMPUP_MEMORY_MAINTAIN_WINDOW` (`02:00-06:00 America/Los_Angeles`) | When rewrites run; `always` or `off`. |
| `AMPUP_MEMORY_MAINTAIN_KEEP` (10), `AMPUP_MEMORY_MAINTAIN_SECS` (600), `AMPUP_MEMORY_MAINTAIN_GRACE_SECS` (120) | Snapshot expiry. |
| `AMPUP_MEMORY_MAINTAIN_PACE` (`2s`), `AMPUP_MEMORY_MAINTAIN_BUDGET` | Pace and budget of the window. |
| `AMPUP_MEMORY_LEDGER_COMPACT=off`, `AMPUP_MEMORY_LEDGER_COMPACT_FILES` (500), `AMPUP_MEMORY_LEDGER_COMPACT_EVERY` (`24h`), `AMPUP_MEMORY_LEDGER_COMPACT_MAX_FILES` (2000) | Ledger compaction. |
| `AMPUP_MEMORY_COMPACT=off`, `AMPUP_MEMORY_COMPACT_FILES` (200), `AMPUP_MEMORY_COMPACT_MAX_FILES` (20000), `AMPUP_MEMORY_COMPACT_MAX_BYTES` (1 GiB) | Table compaction. |
| `AMPUP_MEMORY_PURGE_GRACE` (`7d`) | How long a deleted or aged-out episode stays hidden before a scheduled purge. |
| `AMPUP_MEMORY_DREAM_ON`, `AMPUP_MEMORY_DREAM_SECS`, `AMPUP_MEMORY_DREAM_FACTS` (200), `AMPUP_MEMORY_DREAM_CALLS` (100), `AMPUP_MEMORY_DREAM_PARALLEL` (8), `AMPUP_MEMORY_DREAM_BUDGET` (`10m`) | Dreaming. |

## Identity

| Variable | What it does |
|---|---|
| `AMPUP_MEMORY_API_KEYS` | A keys file: admin keys, and the keys of a server that serves its own scope only (a non-admin file key reads that scope, never an org's; orgs use `ampup-memory apikey`). Read again within 2 seconds when it changes; keep it the same on every server. |
| `AMPUP_MEMORY_JWT_JWKS_URL`, `AMPUP_MEMORY_JWT_PUBLIC_KEY`, `AMPUP_MEMORY_JWT_SECRET`, `AMPUP_MEMORY_JWT_ISSUER`, `AMPUP_MEMORY_JWT_AUDIENCE`, `AMPUP_MEMORY_JWT_ORG_CLAIM`, `AMPUP_MEMORY_JWT_GROUPS_CLAIM`, `AMPUP_MEMORY_JWT_USER_CLAIMS` | JWT callers. |
| `AMPUP_MEMORY_OIDC_ISSUER`, `AMPUP_MEMORY_OIDC_AUDIENCE`, `AMPUP_MEMORY_OIDC_CLI_CLIENT_ID`, `AMPUP_MEMORY_OIDC_CLI_CLIENT_SECRET`, `AMPUP_MEMORY_OIDC_CLI_CALLBACK_PORT` | People's sign-in through the server's identity provider. Serve a client secret only for a public client. |
| `AMPUP_MEMORY_IAP_AUDIENCE` | People through Google's Identity-Aware Proxy. |
| `AMPUP_MEMORY_SECRET_KEY` | 32 bytes (base64 or hex) that seal identity-provider secrets and directory snapshots. Same on every server. Without it every snapshot is checked against the directory log on load. |
| `AMPUP_MEMORY_DIRECTORY=required` | Every caller must select an org. |
| `AMPUP_MEMORY_DISABLED_FILE`, `AMPUP_MEMORY_TEAM_CSV` | A server's own disabled-users list and team csv. |
| `AMPUP_MEMORY_ALLOW_LOCAL_ISSUERS=1` | Allow an identity provider on this machine (development). |
| `AMPUP_MEMORY_AUDIT_FLUSH_SECS` (60) | How often the audit buffer is written. |

## Data handling

| Variable | What it does |
|---|---|
| `AMPUP_MEMORY_TRAIN_LOG` | Keep every model call whole, per org the operator turns on, to train smaller models (`ampup-memory train-log export`). Off unless named. |
| `AMPUP_MEMORY_TELEMETRY=off` | Send and write no telemetry. |
| `SENTRY_DSN`, `SENTRY_ENVIRONMENT` | Error reports. |

## Client and hooks

| Variable | What it does |
|---|---|
| `AMPUP_MEMORY_URL`, `AMPUP_MEMORY_ORG`, `AMPUP_MEMORY_TOKEN`, `AMPUP_MEMORY_SCOPE` | Where the hooks and `ampup-memory mcp` send, and as whom. |
| `AMPUP_MEMORY_CAPTURE=off`, `AMPUP_MEMORY_RECALL=every`, `AMPUP_MEMORY_STARTER=off` | What the hooks send. |
| `AMPUP_MEMORY_CONFIG_DIR` | Where contexts and sign-ins are kept (`~/.ampup-memory`). |
| `AMPUP_MEMORY_CONTEXT` | The context for one command (as `--context`). |
| `AMPUP_MEMORY_LOGIN_PORT` | The loopback port for browser sign-in. |
