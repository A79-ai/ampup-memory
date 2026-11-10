# CLI

Two binaries build from this repository:

- `memstore`: the server, the store commands and the admin CLI (`cargo install --locked --path .`).
- `memstore-client`: what runs on a person's machine (sign-in, coding-agent hooks, the stdio MCP bridge), a few MB
  (`cargo install --locked --path crates/memstore-client`).

The commands will be renamed `ampup-memory` in a later release. `memstore <command> --help` documents every flag.

Global options: `--context <name>` runs one command in another context (or `MEMSTORE_CONTEXT`); `--direct` sends
directory commands to the catalog as its owner even when signed in to a server.

Many store commands take `--local <dir>` to work on a `serve --local` directory instead of the catalog the
environment names. Stop the server first: one process owns a directory at a time.

## Serving

| Command | What it does |
|---|---|
| `memstore serve [scope] [port] [groups] [--local [DIR]] [--workspaces]` | The HTTP and MCP server. `scope` defaults to `org`, `port` to 8080. `--local` keeps everything under one directory (default `~/.memstore`), with no sign-in, bound to 127.0.0.1. |
| `memstore mcp` | The server's MCP endpoint over stdio, signed in as the CLI is. |
| `memstore doctor` | Whether this machine's memory works. Exits non-zero when the server can't be used. |
| `memstore identity check` | What `serve` makes of this environment's identity settings. |

## Writing and reading

| Command | What it does |
|---|---|
| `memstore ingest <scope> <source> <acl> <body>` | Record one episode. |
| `memstore put <scope> <acl> <entity> <predicate> <value>` | Record one fact as its own episode. |
| `memstore close <scope> <fact_id> <when> <acl> <reason…>` | End a fact; the reason is its evidence. |
| `memstore get <scope> <user> [groups…]` | Every fact a reader can see, tab-separated. |
| `memstore search <scope> <user> <text…>` | Text search over facts. |
| `memstore query <scope> <user> <json or @file>` | A typed query ([schemas-and-queries.md](schemas-and-queries.md)). |
| `memstore brief <scope> <user> <subject> [--local DIR]` | Where a subject stands. |
| `memstore load <dir> [--workspace ws] [--branch b] [--fresh] [--local DIR]` | Load a corpus directory. Idempotent. |
| `memstore index <scope> [--wait] [--local DIR]`, `index status\|rebuild\|verify\|list\|drop` | Search indexes. |
| `memstore embed <text…>` | Embed text with the active backend; prints which. |
| `memstore remote search <text>`, `memstore remote query <json>` | The same, against the server you're signed in to. |

## Learning

| Command | What it does |
|---|---|
| `memstore runs load <scope> <file> [--as user] [--local DIR]` | Record a JSON-lines file of runs, scored or not, as one batch. |
| `memstore advice <scope> [--acl …] [--min-wrong N] [--local DIR]` | Learn advice from rewarded runs. |
| `memstore compile <scope> [--sources kb:,…] [--branch b] [--limit N] [--local DIR]` | Compile skills and AIP graphs. |
| `memstore extract [scope] [--limit N] [--dry-run] [--branch b] [--local DIR]` | Extract facts once. |
| `memstore backfill <scope> [--dry-run] [--max-episodes N] [--max-usd X] [--local DIR]` | Read history with the current schema. |
| `memstore aggregate [scope] [--now] [--subject s] [--dry-run] [--show] [--local DIR]` | Summarise subjects. |
| `memstore dream [scope] [--dry-run] [--calls N] [--local DIR]` | Run a dream pass. |
| `memstore qa-log <scope> [--local DIR]` | The answer log: asks, runs, rewards and judgements, as JSON lines. |
| `memstore retract-facts <scope> --fact <id> \| --extractor <actor prefix>` | Take back facts for every reader. |
| `memstore merge-subjects <scope> <from> <into>` (or `--plan file`) | Merge one subject into another. |
| `memstore train-log sweep\|export` | The training log of model calls. |

## Schemas and types

| Command | What it does |
|---|---|
| `memstore schema list` | The built-in schemas. |
| `memstore schema check <name or file>` | Check a schema without a catalog. |
| `memstore schema get <scope> [--branch b] [--local DIR]` | A scope's schema. |
| `memstore schema set <scope> <name or file> [--branch b] [--breaking] [--dry-run] [--local DIR]` | Set it. |
| `memstore episode-type list`, `show <type>`, `units <type\|auto> <file> [--source s] [--quote q]` | Episode types and how they cite. |

## Workspaces

```bash
memstore workspace create exp1 --schema corpus --ttl 7d --max-facts 2000000
memstore load ./corpus --workspace exp1
memstore index org:exp1
memstore workspace snapshot exp1 --label baseline       # prints snap_<16 hex>
memstore workspace branch exp1 arm1
memstore workspace show exp1
memstore serve org 8080 --workspaces                    # every workspace, side by side
memstore workspace delete exp1 --confirm exp1
memstore workspace gc                                   # delete those past their time to live
```

## Coding agents

| Command | What it does |
|---|---|
| `memstore setup <server> [--org o] [--team t] [--harness …] [--dry-run]` | Sign in and install memstore into every coding agent found, then `doctor`. |
| `memstore hooks install\|uninstall --harness codex\|gemini\|cursor\|copilot [--url u] [--dry-run]` | Register or remove the hooks and MCP server. |
| `memstore hook <event> [args…] [--harness h] [--session id]` | A hook (`session-start`, `prompt`, `stop`, `session-end`, `status`, `outcome good\|bad [why]`). |

`memstore-client` has `join`, `hook`, `mcp`, `doctor` and `metric`.

## People, orgs and access

| Command | What it does |
|---|---|
| `memstore create-org <org> [--admin email]` | Create an org and switch to it. |
| `memstore org init [--kind sales\|engineering\|general] [--yes] …` | Set the org's memory profile. |
| `memstore create-team <team>` | Create a team. |
| `memstore invite --email e [--team t] [--role admin\|member]` or `--file people.csv` | Make people members. |
| `memstore requests`, `memstore decline` | Pending requests to join. |
| `memstore join <server> [--org o] [--team t]` | Sign in and add a context. |
| `memstore use <context \| org \| org/team>`, `memstore whoami` | Switch and show the current context. |
| `memstore config get-contexts\|set-context\|use-context\|delete-context\|current-context\|view\|set` | Contexts, like `kubectl config`. |
| `memstore auth login\|status\|logout\|print-token`, `auth setup\|map-group\|domain\|remove-provider` | Sign-in, and an org's own identity provider. |
| `memstore apikey create\|list\|revoke\|sources\|import` | Service-account keys. |
| `memstore org list\|show\|settings\|schema\|set-schema\|sync\|export\|retention\|purge\|delete` | Orgs. |
| `memstore team create\|list\|delete\|add\|remove`, `memstore member add\|remove\|list` | Teams and members. |
| `memstore team check\|keys\|load <team.csv>` | A pilot from a team csv (`email,name,manager,role,team`), with no identity provider. |
| `memstore users disable\|enable\|forget` | Disable or forget a person. |
| `memstore acl set <url> <acl> --source-prefix p \| --source s \| --episode id --key k` | Change episodes' ACLs. |
| `memstore audit [--org o] [--since 7d] [--kind k] [--json]` | The audit log. |
| `memstore keys new\|revoke` | Admin keys in a keys file (other keys: `apikey`). |

## Maintenance and checks

| Command | What it does |
|---|---|
| `memstore maintain <scope> [keep] [--local DIR]` | Expire old Iceberg snapshots now. |
| `memstore compact-ledger [--dry-run] [--aggregates scope] [--local DIR]` | Compact the ledger (or a scope's summaries). |
| `memstore smoke <scope>` | Write and read back a fixture: visibility, as-of reads, branches, merges. |
| `memstore bench <scope> [reads] [gap_ms]` | Read latency, and how soon a write is visible to another reader. |
| `memstore gen <seed> <episodes> <dir>`, `verify-corpus <dir>`, `eval <dir>` | Synthetic corpora and gold questions. |
| `memstore check …` | Engine property checks (concurrent commits, pinning, crashes, clock skew, index equivalence, …). |
| `memstore support-bundle --engine <url> …` | A redacted diagnostics bundle, written locally. |
| `memstore telemetry schema\|check <file>` | The telemetry contract. |
