# Concepts

AmpUp Memory keeps what agents and people tell it, and learns from how their work went. This page names the
parts. The command is `ampup-memory` (`memstore` before the rename, which still works).

## The learning loop

1. **A conversation is a session.** An agent opens one with `start_session` and closes it with `end_session`,
   sending a short transcript and the outcome.
2. **Each task asks for advice.** Before substantive work the agent calls `get_advice` with the task. It gets back
   one skill for that kind of task, with where it came from, or nothing.
3. **A rewarded session teaches the next one.** Ended sessions are judged by a small model, or rewarded
   explicitly (-1 to 1). Rewarded sessions and runs are sorted into kinds of task, and for each kind the memory
   writes a note: the steps that got the rewarded outcome and the pitfalls of the runs that didn't.
4. **The note is served before the next run of that kind of task**, and its evidence (served, applied, helped)
   keeps counting.

Before any runs teach a note, `get_advice` can serve a procedure compiled from your documents (AIP, below).

## Scopes

Memory lives in scopes. Each scope is its own Iceberg namespace, so isolation is enforced by the catalog, not by a
filter.

| Scope | Written on the command line as |
|---|---|
| The default org | `org` |
| An org | `org:<org>` |
| A team of the default org | `team:<id>` |
| A team of an org | `team:<org>/<team>` |
| A user | `user:<id>` |
| A person's own memory beside their org's | `?scope=mine` on any HTTP route |

Org and team names are lower-case letters, digits and single underscores.

## Episodes

An episode is one source record: a note, a call transcript, a Slack thread, an email, a CRM change, a document.

- Its id is the SHA-256 of its body (or of its source and body), so the same text from the same source is the
  same episode, and a retried write changes nothing.
- It carries a `source` (`slack:C09:1712.3`, `gmail:…`, `mcp:remember_memory:<author>:<source>` for a note), an
  `occurred_at` time, an optional `about` list of entity ids, and an ACL.
- Bodies are stored once, content-addressed, beside the episodes table.
- Every write names an **episode type**, chosen by source prefix:

| Type | Sources |
|---|---|
| `sales_call` | `meeting:`, `gong:`, `zoom:`, `recall:`, `ampup:meeting:` |
| `slack_thread` | `slack:` |
| `email` | `gmail:`, `outlook:`, `email:` |
| `crm_update` | `hubspot:`, `salesforce:`, `dynamicsCRM:`, `crm:` |
| `doc` | `mcp:remember_memory:` (notes), `github:`, `linear:`, `sentry:`, `granola:`, `doc:`, `note:` |

A write whose source no type claims is refused. A corpus loaded with `ampup-memory load` may use any source; unclaimed
ones read as `doc`. Each type says how its body splits into the units facts cite (a call's turn, a message, a
paragraph, a CRM field) and which predicates a model may extract from it (`ampup-memory episode-type show <type>`).

### Documents kept in versions

An episode can be one version of a document (`"document": key`). A reader sees the newest version they may read
that occurred by the time asked. `get_episode {document}` and `POST /document` read by key.

## Facts

A fact is `(entity, predicate, value)` with a valid-time window, a verbatim quote and the episodes it cites.

- Facts come from extraction (a model reads episodes), from `POST /facts`, or from a loaded corpus.
- A reader sees a fact only if they may read **every** episode it cites.
- A one-valued attribute's newest visible version wins at read time, so writers need not close old values.
- A **closure** ends a value with no successor (`ampup-memory close`, `POST /close`, MCP `close_memory`). Its reason is
  its own episode and its evidence. Nothing is deleted: `as_of` reads still show the old value.
- Each fact records who wrote it (`written_by`: `extract:<model>` or `posted`).

## Commits, the ledger and branches

- A write is a commit. Episode and fact rows land first, then one row in `memory_system.ledger`. A row is visible
  only once its ledger row has landed, so a crash mid-commit leaves nothing visible.
- The ledger append is a compare-and-set on the ledger table's snapshot, so the history stays one chain under
  concurrent writers.
- Branches work as in git. `merge` compares per (entity, predicate) against the branch base and refuses on a
  conflict instead of picking a last write.
- A **workspace** is a disposable org for an experiment, a demo or a test, with its own scopes, caps and time to
  live. A **workspace snapshot** pins every scope and branch of it under one id (`snap_…`); a read pinned to it
  sees exactly that memory. A **pin** (`POST /pin`, `pin_…`) does the same for any scope for a leased time, for
  long-running agents.

## Search

- **Facts.** `search_memory` and `POST /search` rank facts by keyword (BM25F) and meaning (embedding cosine),
  combined 0.6 keyword / 0.4 meaning. A query that names a time ranks facts stated in that window first.
- **Passages.** Episode bodies are cut into chunks of about 1,500 bytes and indexed too. `query_memory` /
  `POST /query` with `{"find": "passages"}` searches them. Passage search works with no extraction model at all.
- `search_memory`'s text answer groups facts by entity, cites each as `[e1 t3]` (episode 1, turn 3) and ends
  with the episode ids `get_episode` reads. It also shows matching source text no fact cites.

The index is derived. It lives at `AMPUP_MEMORY_INDEX_URI` and can be rebuilt from the tables at any time
(`ampup-memory index rebuild`). See [schemas-and-queries.md](schemas-and-queries.md).

## Schemas

A schema types a scope: entity types, their attributes, value types (`string`, `text`, `enum`, `number`, `money`,
`date`, `timestamp`, `bool`, `ref`) and cardinality. Schemas are JSON data, committed through the ledger. Three
ship built in: `sales`, `engineering` and `corpus` (in `schemas/`). A schema's `extract` section says which
episodes a model reads into typed facts. See [schemas-and-queries.md](schemas-and-queries.md).

## Advice

- **Runs.** An agent records a run (`record_run`, `POST /runs`: task, trace, outcome, the advice key it was
  served) and rewards it once the outcome is known (`reward_run`, `POST /runs/reward`, -1 to 1 with a reason).
- **Sessions.** A session is a conversation from start to end. Its advice steps, transcript and outcome are kept,
  and a judge model says per note whether it was applied and whether it helped.
- **Situations.** Rewarded runs and sessions are sorted into a two-level map, category then situation, by the
  kind of work their task needs. No labels are given; the map is the scope's own.
- **Notes.** For each situation with enough runs, a learning model writes a note: *When*, *Steps*, *Pitfalls*,
  *Evidence*.
- **Procedures (AIP).** A scope's chosen documents are compiled into skills (when, steps, edge cases, sources)
  and each skill into an AIP step graph.
- **Evidence gate.** A new team note is a candidate. It is served only in a trial share of sessions until it has
  helped enough different people (3 by default).

The full rules are in [advice.md](advice.md).

## Who reads what

Every episode carries a short flat ACL: `*`, `user:<id>`, `group:<id>`, `role:<name>`. A caller is a principal: a
user plus the groups they hold, flattened by your identity provider or directory. Visibility is a bitmap per ACL
entry, so a membership change touches one caller, never the episodes. See [security-model.md](security-model.md).

## What runs in the background

`ampup-memory serve` does, on timers, what the matching commands do once:

| Work | Command | Timer |
|---|---|---|
| Extract facts | `ampup-memory extract` | `AMPUP_MEMORY_EXTRACT_SECS` (5 s) |
| Compile skills and AIP graphs | `ampup-memory compile` | `AMPUP_MEMORY_COMPILE_SECS` (60 s) |
| Learn advice | `ampup-memory advice` | `AMPUP_MEMORY_ADVICE_SECS` (600 s) |
| End idle sessions, judge ended ones | | `AMPUP_MEMORY_SESSION_SWEEP_SECS` (60 s) |
| Summarise subjects | `ampup-memory aggregate` | `AMPUP_MEMORY_AGGREGATE_SECS` (3600 s) |
| Dream (reconcile facts) | `ampup-memory dream` | hourly, on scopes `AMPUP_MEMORY_DREAM_ON` names |
| Compact tables, expire snapshots | `ampup-memory compact-ledger`, `ampup-memory maintain` | nightly window `AMPUP_MEMORY_MAINTAIN_WINDOW` |

With no model configured, none of the model-backed work runs: memory is all there is, and no model is called.
