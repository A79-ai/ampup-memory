# Schemas, extraction and queries

## Schemas

A schema types a scope. It declares entity types, their attributes, each attribute's value type (`string`, `text`,
`enum`, `number`, `money`, `date`, `timestamp`, `bool`, or `ref` to another entity) and cardinality (`one`, the
default, where a newer value replaces the older; or `many`, where values accumulate).

- A `ref` may be marked `part_of` (a meeting is part of its deal): passage search treats passages about the part
  as about the whole.
- A `many` attribute may be marked `"list": true` when its values are held at once (the tools a customer uses).
- `guidance` lines tell agents how the domain's questions map onto the types; they appear in `query_memory`'s
  description.
- `metrics` name stored `count` queries an agent can run by name.
- Three ship built in: `sales`, `engineering`, `corpus` (`schemas/`). Any JSON file in the same shape works.

```bash
memstore schema list                                      # the built-in schemas
memstore schema check my-schema.json                      # check a file, no catalog needed
memstore schema set org:acme sales                        # a built-in name, or a file
memstore schema set org:acme my-schema.json --branch try  # try it on a branch; main keeps its own
memstore schema set org:acme my-schema.json --dry-run     # every check a set would make; commit nothing
memstore schema set org:acme my-schema.json --breaking    # a breaking change on main (raise `version` too)
memstore schema get org:acme
memstore schema set org ./my-schema.json --local ~/.memstore   # a laptop store (server stopped)
```

A schema is committed through the ledger like any write. A scope without one stays untyped. Additive changes (new
types, predicates, enum values, a wider `ref`) apply in place. A change that would read stored facts differently
is breaking: on main it needs a higher `version` and `--breaking`.

Writes to a typed scope are checked: the entity id names its type (`deal:42`), the attribute is declared (unless
the schema's `mode` is `open`), and the value parses (`$120,000` is stored as `120000`).

## Extraction

A model reads episodes into facts. Which episodes:

- **What the schema's `extract` section names** (goal-oriented extraction). Each episode is read with the
  schema's types and its episode type's catalog; each fact cites the unit it comes from.
- **Starter documents** (a repo's `CLAUDE.md` and rules), always.
- **Everything else**, goal-blind, only where turned on: `set_settings {extract: true}` for an org or team, or
  `MEMSTORE_EXTRACT_GOAL_BLIND=1` for the whole server. On `serve --local` use the variable.

No episode is read by both. The goal-blind prompt keeps nine kinds of fact (decision, issue, owner, how_it_works,
how_to_enable, supports, shipped_in, changed_in, requested_by), and a fact is kept only if its quote, at least 12
characters, occurs in the text.

```json
{"name": "deals", "description": "Know where every deal stands",
 "guidance": ["Keep what the buyer says hurts, and who else they look at."],
 "types": {"deal": {"predicates": {"budget_note": {"type": "string", "description": "the budget as stated"}}}},
 "extract": {"sources": {"prefixes": ["gmail:"], "about": ["deal"]}, "facts_per_episode": 40,
             "aggregates": {"subject": "deal", "elements": {"pain": ["pain"], "competition": ["competitor"]}}}}
```

- `sources` takes `prefixes`, entity types the episodes are `about`, and `from` / `to` times. `"sources": {}` is
  every episode.
- `facts_per_episode` caps one episode's facts.
- `"outcomes": true` (or a list of source prefixes) also asks for what the text reports happened, dated.
- `"style": "statements"` writes each fact as one self-contained sentence under one subject, its predicate a kind
  (`decision`, `commitment`, `plan`, `fix`, …).
- Each episode is read once per schema version. `memstore backfill <scope>` reads the history with the schema as it
  stands (`--dry-run` estimates tokens and dollars with `MEMSTORE_EXTRACT_PRICE`; `--max-episodes`, `--max-usd`).
- `memstore extract [scope] [--limit N] [--dry-run] [--branch b] [--local DIR]` runs once. `--dry-run` prints what
  it would add and end, one JSON line per episode: audit it before turning extraction on over real data.

New facts are reconciled with what is known before they are committed. The model sees only facts every reader of
the new episode may already read. A repeat is dropped; a fact that supersedes an old one ends it with a closure
citing the new episode. Nothing is deleted.

An episode the model refuses is tried three times, then given up on. While the API is down or refuses the key,
nothing is given up on and the worker backs off. Refused facts are counted on `/metrics`
(`memstore_extraction_refused_facts_total{reason}`) and in `GET /doctor`.

Entity names are normalised: `BillingExport`, `billing-export` and `the billing export` are all `billing export`.

## Typed queries

Queries are JSON, the same over `POST /query`, MCP `query_memory` and `memstore query <scope> <user> <json>`:

```json
{"type": "deal",
 "where": [{"attr": "stage", "in": ["proposal", "negotiation"]}, {"attr": "amount", "gte": 50000},
           {"rel": "account", "to": {"where": [{"attr": "industry", "eq": "insurance"}]}}],
 "select": ["stage", "amount", "owner.name"], "order": [{"by": "amount", "dir": "desc"}]}
```

- `find`: `entities` (the default), `facts`, `count` (with `group_by` and `metrics` such as `{"sum": "amount"}`),
  `changes` (what started or ended in a window), `passages`, or `schema`.
- `where` clauses all hold: `{"attr", <op>: v}` with `eq ne in gt gte lt lte between exists missing contains
  older_than within`; `{"rel", "to": query}`; `{"rel_from", "from": query}` (two hops at most); `{"any": [...]}`;
  `{"not": clause}`.
- `id`, `match` (`{"name": "acme"}` resolves names and aliases), `text`, `select` (`owner.name` follows a
  reference), `order`, `include: ["evidence"]`, `limit` (at most 500) and `cursor`.
- `{"metric": name}` runs one of the schema's named metrics.
- `time`: `now` (the default), `all`, `{"as_of": T}` or `{"between": [T1, T2]}`.
- A cursor reads the memory its first page read (branch heads, time, index versions), so pages never repeat or
  skip a row when writes land between them.
- A mistake comes back as a reason: `type deal has no attribute "colour" (attributes: …)`.

### Passages

```json
{"find": "passages", "text": "security review",
 "about": {"type": "deal", "where": [{"rel": "account", "to": {"where": [{"attr": "industry", "eq": "insurance"}]}}]},
 "evidence": {"source": ["email", "call"], "occurred": {"gte": "2026-09-01"}}, "limit": 10}
```

- Bodies are cut into chunks of about 1,500 bytes (`MEMSTORE_CHUNK_BYTES`, `MEMSTORE_CHUNK_OVERLAP` 150) at
  paragraph or sentence boundaries.
- Each episode ranks by its best chunk, so one long transcript can't fill the page. A page spreads over kinds of
  source (`"diverse": false` turns that off). Newer sources win near-ties (`"recency": false` turns that off).
- `{"find": "passages", "id": [...]}` reads up to ten passages or episodes by id.
- `group_by` a type rolls hits up to the entities they are about.

## How fact search ranks

- **Keyword:** BM25F over the fact's value (weight 1), subject (0.6), quote (0.5), predicate and date (0.2).
- **Meaning:** cosine similarity of embeddings.
- The two are min-max normalised and combined 0.6 keyword / 0.4 meaning (reciprocal-rank fusion when a list has
  fewer than 3 hits). Age doesn't scale scores.
- **Time:** a query that names a time (`Q3 2026`, `last year`, `2026-03-02`) also takes facts stated in that
  window as candidates and ranks them first.
- A search takes the best 300 rows of each list, so its first hits are the same at every `k`.
- `MEMSTORE_RANK_<knob>` overrides each weight for experiments.

## Supersession, closures and stale values

- Under `now` and `as_of T`, a one-valued attribute's current version is the newest one the reader can see. This is
  decided per reader: a newer version from an episode they can't see doesn't hide the older one.
- A closure ends a value with no successor; it applies only to readers who can see its evidence.
- A schema can say where a current value may no longer be true:

  ```json
  "stage":      {"type": "enum", "freshness_days": 45},
  "close_date": {"type": "date", "plan": true},
  "status":     {"type": "enum", "values": ["open", "won", "lost"], "pending": ["open"]}
  ```

  With `freshness: true` on an org or team (or `MEMSTORE_FRESHNESS=1`), reads mark a value a newer fact restates
  (`later on <date>: "…"`) and a plan whose date passed while still pending. `MEMSTORE_FRESHNESS_MARKS` (`later`,
  `overdue`, `stale`) picks the kinds. Marks are derived at read time; nothing is written. Off by default.

## Aggregation and briefs

`extract.aggregates` summarises extracted facts per subject: each element scored (none, identified, partial,
confirmed) with the facts behind it, items given a status (`open`, `done`, …), stale facts and passed plans
flagged.

```json
"aggregates": {
  "subject": "deal",
  "elements": {"meddpicc.metrics": ["close_rate", "avg_deal_size"], "meddpicc.champion": ["advocate", "title"]},
  "items": {"commitment": ["open", "done", "dropped"]},
  "rules": ["A follow-up saying something was sent completes the commitment to send it."],
  "every": 10, "max_wait_minutes": 60
}
```

- A subject is summarised again after `every` new episodes about it, or after `max_wait_minutes`. `serve` checks
  hourly (`MEMSTORE_AGGREGATE_SECS`). `memstore aggregate [scope] [--now] [--subject s] [--dry-run]` runs it now.
- One model call per batch (`MEMSTORE_AGGREGATE_MODEL`, else the extraction model); every answer cites fact ids.
- A row is readable by whoever may read every episode of its batch.
- `get_brief`, `POST /brief {subject, refresh?}` and `memstore brief <scope> <user> <subject>` read where a
  subject stands in one answer of at most 4,000 tokens.

## Dreaming (opt-in)

A dream pass catches what write-time reconcile misses: an older value backfilled after a newer one, the same
attribute under two predicate names, two facts saying the same thing, one thing under several names.

- It writes only new rows: closures, links between duplicate facts, `_alias` facts. Nothing is deleted.
- It runs per shared audience (`*`, `group:`, `role:`), widest first, so the model never sees another audience's
  facts. A person's own memory isn't dreamed over.
- Code checks every decision: a list-valued or role predicate is never ended, a record of an event is never ended.
- It runs only on scopes `MEMSTORE_DREAM_ON` names, once a day (`MEMSTORE_DREAM_SECS`) when at least
  `MEMSTORE_DREAM_FACTS` (200) new facts wait, within `MEMSTORE_DREAM_CALLS` (100) model calls.
- `memstore dream [scope] [--dry-run] [--calls N] [--local DIR]` runs a pass now.
- Undo: `memstore retract-facts <scope> --extractor dream:<model>@s2` takes back everything a dreamer wrote.

## Indexes

Each scope has a derived index at `MEMSTORE_INDEX_URI` (a path, `gs://…` or `s3://…`; default
`~/.cache/memstore/index`, or `<dir>/index` under `serve --local`). It holds the open facts, the closed (superseded)
facts and the passages, as Lance datasets. Rows are trusted only for data files still in the table's current
snapshot, so a deleted row is never served.

```bash
memstore index org:acme                          # bring the indexes up to date
memstore index status org:acme                   # covered heads, the unindexed tail, the configuration
memstore index org:acme --wait --timeout 600     # wait for a server's background update
memstore index rebuild org:acme --to /tmp/rebuilt
memstore index verify org:acme                   # incremental index against a rebuild
memstore index list org:acme
memstore index drop org:acme <dir>               # an index of another configuration
```

A change of embedding model or chunking builds a new index beside the old one; switching back finds the old one.

## Embeddings

| `MEMSTORE_EMBED` | Backend | Default model |
|---|---|---|
| `local` | fastembed in-process; files fetched once into `MEMSTORE_MODEL_DIR` (`~/.cache/memstore/models`) | `bge-small-en-v1.5`, 384 dims |
| `openai` | OpenAI's embeddings API or any server with the same `/embeddings` shape (`MEMSTORE_EMBED_URL`) | `text-embedding-3-small`, 1,536 dims |
| `openrouter` | OpenRouter's embeddings API with `OPENROUTER_API_KEY` | `openai/text-embedding-3-small`, 1,536 dims |

Unset: `openai` with `MEMSTORE_EMBED_KEY`/`_KEY_FILE`, else `openrouter` with `OPENROUTER_API_KEY`, else `openai`
with `OPENAI_API_KEY`, else `local`. `cargo run` and `cargo test` pin `local` (`.cargo/config.toml`).
`MEMSTORE_EMBED_MODEL` and `MEMSTORE_EMBED_DIMS` override the model. `memstore embed <text>` prints which backend
is active.

`MEMSTORE_EMBED_CACHE` (a directory, `gs://`, `s3://`, `file://` or `off`) keeps every vector by the SHA-256 of
its text, so the same text is embedded once wherever it is indexed. `serve --local` keeps it in
`<dir>/embeddings`; other processes keep none unless it is set.

## Loading a corpus

`memstore load <dir>` loads `episodes*.jsonl`, `facts*.jsonl` and `closures*.jsonl` (facts stand in for
extraction). It is idempotent: an episode or fact already committed is skipped. `--workspace`, `--branch`, `--fresh`
and `--local` apply. `memstore gen <seed> <episodes> <dir>` writes a synthetic corpus with gold questions, and
`memstore eval <dir>` answers them.
