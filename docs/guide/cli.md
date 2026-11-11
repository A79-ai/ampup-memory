# CLI

Two binaries build from this repository:

- `ampup-memory`: the server, the store commands and the admin CLI (`cargo install --locked --path .`).
- `ampup-memory-client`: what runs on a person's machine (sign-in, coding-agent hooks, the stdio MCP bridge), a few MB
  (`cargo install --locked --path crates/memstore-client`).

The commands were `memstore` and `memstore-client` before the rename, and those names still work, as links. `ampup-memory <command> --help` documents every flag.

Global options: `--context <name>` runs one command in another context (or `AMPUP_MEMORY_CONTEXT`); `--direct` sends
directory commands to the catalog as its owner even when signed in to a server.

Many store commands take `--local <dir>` to work on a `serve --local` directory instead of the catalog the
environment names. Stop the server first: one process owns a directory at a time.

## Serving

| Command | What it does |
|---|---|
| `ampup-memory serve [scope] [port] [groups] [--local [DIR]] [--workspaces]` | The HTTP and MCP server. `scope` defaults to `org`, `port` to 8080. `--local` keeps everything under one directory (default `~/.ampup-memory`), with no sign-in, bound to 127.0.0.1. |
| `ampup-memory mcp` | The server's MCP endpoint over stdio, signed in as the CLI is. |
| `ampup-memory doctor` | Whether this machine's memory works. Exits non-zero when the server can't be used. |
| `ampup-memory identity check` | What `serve` makes of this environment's identity settings. |

## Writing and reading

| Command | What it does |
|---|---|
| `ampup-memory ingest <scope> <source> <acl> <body>` | Record one episode. |
| `ampup-memory put <scope> <acl> <entity> <predicate> <value>` | Record one fact as its own episode. |
| `ampup-memory close <scope> <fact_id> <when> <acl> <reason…>` | End a fact; the reason is its evidence. |
| `ampup-memory get <scope> <user> [groups…]` | Every fact a reader can see, tab-separated. |
| `ampup-memory search <scope> <user> <text…>` | Text search over facts. |
| `ampup-memory query <scope> <user> <json or @file>` | A typed query ([schemas-and-queries.md](schemas-and-queries.md)). |
| `ampup-memory brief <scope> <user> <subject> [--local DIR]` | Where a subject stands. |
| `ampup-memory load <dir> [--workspace ws] [--branch b] [--fresh] [--local DIR]` | Load a corpus directory. Idempotent. |
| `ampup-memory index <scope> [--wait] [--local DIR]`, `index status\|rebuild\|verify\|list\|drop` | Search indexes. |
| `ampup-memory embed <text…>` | Embed text with the active backend; prints which. |
| `ampup-memory remote search <text>`, `ampup-memory remote query <json>` | The same, against the server you're signed in to. |

## Learning

| Command | What it does |
|---|---|
| `ampup-memory runs load <scope> <file> [--as user] [--local DIR]` | Record a JSON-lines file of runs, scored or not, as one batch. |
| `ampup-memory advice <scope> [--acl …] [--min-wrong N] [--local DIR]` | Learn advice from rewarded runs. |
| `ampup-memory compile <scope> [--sources kb:,…] [--branch b] [--limit N] [--local DIR]` | Compile skills and AIP graphs. |
| `ampup-memory extract [scope] [--limit N] [--dry-run] [--branch b] [--local DIR]` | Extract facts once. |
| `ampup-memory backfill <scope> [--dry-run] [--max-episodes N] [--max-usd X] [--local DIR]` | Read history with the current schema. |
| `ampup-memory aggregate [scope] [--now] [--subject s] [--dry-run] [--show] [--local DIR]` | Summarise subjects. |
| `ampup-memory dream [scope] [--dry-run] [--calls N] [--local DIR]` | Run a dream pass. |
| `ampup-memory qa-log <scope> [--local DIR]` | The answer log: asks, runs, rewards and judgements, as JSON lines. |
| `ampup-memory retract-facts <scope> --fact <id> \| --extractor <actor prefix>` | Take back facts for every reader. |
| `ampup-memory merge-subjects <scope> <from> <into>` (or `--plan file`) | Merge one subject into another. |
| `ampup-memory train-log sweep\|export` | The training log of model calls. |

## Schemas and types

| Command | What it does |
|---|---|
| `ampup-memory schema list` | The built-in schemas. |
| `ampup-memory schema check <name or file>` | Check a schema without a catalog. |
| `ampup-memory schema get <scope> [--branch b] [--local DIR]` | A scope's schema. |
| `ampup-memory schema set <scope> <name or file> [--branch b] [--breaking] [--dry-run] [--local DIR]` | Set it. |
| `ampup-memory episode-type list`, `show <type>`, `units <type\|auto> <file> [--source s] [--quote q]` | Episode types and how they cite. |

## Workspaces

```bash
ampup-memory workspace create exp1 --schema corpus --ttl 7d --max-facts 2000000
ampup-memory load ./corpus --workspace exp1
ampup-memory index org:exp1
ampup-memory workspace snapshot exp1 --label baseline       # prints snap_<16 hex>
ampup-memory workspace branch exp1 arm1
ampup-memory workspace show exp1
ampup-memory serve org 8080 --workspaces                    # every workspace, side by side
ampup-memory workspace delete exp1 --confirm exp1
ampup-memory workspace gc                                   # delete those past their time to live
```

## Coding agents

| Command | What it does |
|---|---|
| `ampup-memory setup <server> [--org o] [--team t] [--harness …] [--dry-run]` | Sign in and install AmpUp Memory into every coding agent found, then `doctor`. |
| `ampup-memory hooks install\|uninstall --harness codex\|gemini\|cursor\|copilot [--url u] [--dry-run]` | Register or remove the hooks and MCP server. |
| `ampup-memory hook <event> [args…] [--harness h] [--session id]` | A hook (`session-start`, `prompt`, `stop`, `session-end`, `status`, `outcome good\|bad [why]`). |

`ampup-memory-client` has `join`, `hook`, `mcp`, `doctor` and `metric`.

## People, orgs and access

| Command | What it does |
|---|---|
| `ampup-memory create-org <org> [--admin email]` | Create an org and switch to it. |
| `ampup-memory org init [--kind sales\|engineering\|general] [--yes] …` | Set the org's memory profile. |
| `ampup-memory create-team <team>` | Create a team. |
| `ampup-memory invite --email e [--team t] [--role admin\|member]` or `--file people.csv` | Make people members. |
| `ampup-memory requests`, `ampup-memory decline` | Pending requests to join. |
| `ampup-memory join <server> [--org o] [--team t]` | Sign in and add a context. |
| `ampup-memory use <context \| org \| org/team>`, `ampup-memory whoami` | Switch and show the current context. |
| `ampup-memory config get-contexts\|set-context\|use-context\|delete-context\|current-context\|view\|set` | Contexts, like `kubectl config`. |
| `ampup-memory auth login\|status\|logout\|print-token`, `auth setup\|map-group\|domain\|remove-provider` | Sign-in, and an org's own identity provider. |
| `ampup-memory apikey create\|list\|revoke\|sources\|import` | Service-account keys. |
| `ampup-memory org list\|show\|settings\|schema\|set-schema\|sync\|export\|retention\|purge\|delete` | Orgs. |
| `ampup-memory team create\|list\|delete\|add\|remove`, `ampup-memory member add\|remove\|list` | Teams and members. |
| `ampup-memory team check\|keys\|load <team.csv>` | A pilot from a team csv (`email,name,manager,role,team`), with no identity provider. |
| `ampup-memory users disable\|enable\|forget` | Disable or forget a person. |
| `ampup-memory acl set <url> <acl> --source-prefix p \| --source s \| --episode id --key k` | Change episodes' ACLs. |
| `ampup-memory audit [--org o] [--since 7d] [--kind k] [--json]` | The audit log. |
| `ampup-memory keys new\|revoke` | Keys in a keys file: admin keys, and a single-scope server's (an org's keys: `apikey`). |

## Maintenance and checks

| Command | What it does |
|---|---|
| `ampup-memory maintain <scope> [keep] [--local DIR]` | Expire old Iceberg snapshots now. |
| `ampup-memory compact-ledger [--dry-run] [--aggregates scope] [--local DIR]` | Compact the ledger (or a scope's summaries). |
| `ampup-memory smoke <scope>` | Write and read back a fixture: visibility, as-of reads, branches, merges. |
| `ampup-memory bench <scope> [reads] [gap_ms]` | Read latency, and how soon a write is visible to another reader. |
| `ampup-memory gen <seed> <episodes> <dir>`, `verify-corpus <dir>`, `eval <dir>` | Synthetic corpora and gold questions. |
| `ampup-memory check …` | Engine property checks (concurrent commits, pinning, crashes, clock skew, index equivalence, …). |
| `ampup-memory support-bundle --engine <url> …` | A redacted diagnostics bundle, written locally. |
| `ampup-memory telemetry schema\|check <file>` | The telemetry contract. |
