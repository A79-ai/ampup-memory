# HTTP API

`ampup-memory serve` listens on port 8080 by default (`ampup-memory serve [scope] [port]`). Bodies are JSON. On a server
with sign-in, send `Authorization: Bearer <token or msk_ key>` and select memory with `x-memstore-org` and
`x-memstore-team` headers (or `?org=&team=`). A client that isn't an MCP client can use a member's personal key
(`ampup-memory apikey create --personal`), which acts as them on the memory routes below
([security-model.md](security-model.md#personal-keys)). `serve --local` takes no credentials.

Examples below run against `ampup-memory serve --local` on `localhost:8080`.

## The learning loop

| Route | Body | Answer |
|---|---|---|
| `POST /remember` | `{source, body, acl?, mine?, document?}` | `{episode_id, scope, readers}` |
| `POST /sessions` | `{context?, harness?, max_chars?}` | `{session_id, situation, advice, evidence, skills}` |
| `POST /advice` | `{question, session_id?, moment?, conversation_so_far?, format?, max_chars?, procedures?, notes?}` | `{situation, advice, skills}`; 503 with no routing model |
| `POST /sessions/end` | `{session_id, transcript?, outcome?, reward?, reason?}` | `{ended, served, session_id}` |
| `POST /runs` | `{task, trace, used?, outcome?, advice?}` or `{runs: [...]}` | `{run_id}` or `{run_ids}` |
| `POST /runs/reward` | `{run_id, reward, reason?}` or `{rewards: [...]}` | `{run_id}` or `{run_ids}` |
| `POST /sessions/prs` | `{repo, number, branch?, event, at?, by?}` | how many sessions it reached |
| `POST /advice/seed` | `{repo, sources: [{name, text}]}` or `{check: true}` | `{advises, sent, written, retired, writing_notes}`, or `{advises: false, why}` with no model |

```bash
curl -s localhost:8080/remember -d '{"source": "note:deploy", "body": "Deploy billing with ./deploy.sh billing. Never from a laptop on Fridays."}'
# {"episode_id":"9b26…","readers":"everyone who reads org","scope":"org"}

curl -s localhost:8080/sessions -d '{"context": "deploy the billing service", "harness": "curl"}'
# {"advice":null,"session_id":"6512edd0-…","situation":null}

curl -s localhost:8080/sessions/end -d '{"session_id": "6512edd0-…", "transcript": "Deployed billing with ./deploy.sh.", "outcome": "deployed", "reward": 1}'
# {"ended":true,"served":[],"session_id":"6512edd0-…"}
```

- `/remember`: `acl` entries must be ones the caller holds. Unset, the note is readable by everyone who reads the
  scope; `"mine": true` makes it the caller's alone (refused for a caller with no `user:` entry). The episode's
  source records its author as `mcp:remember_memory:<caller>:<source>`. `document` (a key under `starter/`)
  writes a new version of that document.
- `/advice/seed`: a repository's docs and pull requests, kept as the caller's own episodes; the server writes
  notes for coding tasks from them in the background ([advice.md](advice.md#advice-from-a-repository-from-the-first-session)).
- `/sessions/end` and `/advice` in a session: only the person who started the session may use it; anyone else
  gets 404.
- `/sessions/prs` reports what became of a pull request a session opened or pushed to (`merged`, `closed`,
  `reverted`, `regression`). A revert or a regression marks its sessions failed; a merge marks them succeeded
  (0.5, then 1 once it has stood 14 days).

Details of advice, runs and sessions: [advice.md](advice.md).

## Reading

| Route | Body | What it reads |
|---|---|---|
| `POST /search` | `{"data": [[row, query, k?, valid_at_µs?, snippet_chars?, …]]}` | Facts, ranked by keyword and meaning (`k` default 10; a negative `valid_at` searches all history) |
| `POST /query` | `{"data": [[row, <query JSON>]]}` | Typed queries, passages, metrics ([schemas-and-queries.md](schemas-and-queries.md)) |
| `POST /episode` | `{"data": [[row, episode_id, near_text?, max_chars?]]}` | An episode's text |
| `POST /document` | `{"data": [[row, key, at_µs?, max?]]}` | A document's version at a time; a key ending in `/` lists |
| `POST /browse` | `{op, path?, …}` | Memory as file trees: `sources/`, `entities/`, `skills/` |
| `POST /facts/list` | `{entity?, entity_type?, predicate?, source?, tags?, status?, text?, limit?, cursor?}` | Facts the caller may read, newest first |
| `POST /advice/list` | `{space?, category?, situation?, text?, …}` | Situations whose advice the caller may read, with evidence tallies |
| `POST /lessons` | `{space?, person?, key?, since?, limit?}` | What it learned: each lesson with the runs it came from and the sessions it was served in, and how each went |
| `POST /brief` | `{subject, refresh?}` | Where a subject stands (needs `extract.aggregates`) |
| `POST /pin` | `{ttl_secs?}`, `{renew}`, `{release}` | A leased pin; read through it with `x-memstore-at: pin_…` or `?at=` |
| `GET /me` | | The caller, their groups, selected scope, memberships, the org's memory profile |
| `GET /doctor` | | What the memory runs for this caller: extraction, compiling, advice and its evidence bar |
| `GET /ready` | | 503 until warm-up is done, then 200 |

The read routes use a row protocol so the same endpoints serve as Snowflake service functions
(`{"data": [[row, args…]]}`, answered as `{"data": [[row, result]]}`) and BigQuery remote functions
(`{"calls": [[args…]]}`, answered as `{"replies": [...]}`).

```bash
curl -s localhost:8080/query -d '{"data": [[0, {"find": "passages", "text": "deploy billing", "limit": 3}]]}'
# {"data":[[0,{"find":"passages","results":[{"id":"9b26c2aadfb65ce3#0","snippet":"Deploy billing with …", …}], …}]]}

curl -s localhost:8080/search -d '{"data": [[0, "how do we deploy billing", 5]]}'
# {"data":[[0,[]]]}   facts only: empty until a model has extracted some
```

`/browse` operations: `ls` (a folder's children, paged with `limit` and `after`), `episode` (text and the facts
citing it), `branches`, `schema`, `facts`, `advice`, `scopes`. A path is a JSON array of segments:

```bash
curl -s localhost:8080/browse -d '{"op": "ls", "path": ["sources"]}'
curl -s localhost:8080/browse -d '{"op": "ls", "path": ["entities", "deal", "42"]}'
```

### What it learned: `/lessons`

`POST /lessons` (also `/browse {"op": "lessons"}`) answers each learned lesson (an advice note: the caller's own,
`space: "mine"`, or the team's) with its trail, newest first. The Explorer's Learned tab reads it, and anything that
reports what the memory learned (a weekly digest) can read the same:

```bash
curl -s localhost:8080/lessons -d '{"since": "2026-11-06"}'
```

```json
{"reader": {"user": "amy@example.com", "everyone": false}, "people": ["amy@example.com"], "count": 1,
 "lessons": [{
   "key": "service-deployment/deploy-a-service-change-to-production", "space": "team", "owner": null,
   "category": "Service deployment", "situation": "Deploy a service change to production",
   "title": "Deploy a service change to production", "when": "…", "lesson": "## When\n…\n## Steps\n1. [how-to] …",
   "learned_at": 1794610311969569, "note_id": "16a4…", "status": "served", "evidence": {"served": 3, "helped": 2, "ratified": false, …},
   "learned_from": {"runs": 11, "right": 6, "wrong": 5, "people": 4, "more": 0, "hidden": 6,
     "trail": [{"id": "…", "kind": "run", "user": "amy@example.com", "question": "Deploy the search service",
                "at": 1794610300000000, "runs": 1, "right": 1, "wrong": 0, "outcome": "right", "by": "reward"}]},
   "served_in": {"sessions": 3, "people": 1, "helped": 2, "good": 2, "bad": 1, "more": 0, "hidden": 0,
     "trail": [{"id": "…", "user": "amy@example.com", "question": "Ship the payments hotfix", "at": 1794610400000000,
                "moment": "start", "harness": "claude-code", "applied": true, "helped": true, "outcome": "good",
                "by": "reward", "reward": 1, "prs": [], "ended_at": 1794610410000000}]}
 }]}
```

- `lesson` is the note as agents are served it (no item ids, no Evidence or Freshness), markdown; `title` is its
  heading. `status`: `served`, `held`, `needs backing`, `withheld` or `not served`.
- `learned_from`: the runs and sessions its situation was learned from (a session's graded requests count as runs of
  it). `served_in`: the sessions it was handed in since, `moment` where (`start`, `prompt`, `agent`, `next`, `fork`,
  `stuck`, `done`), and `helped` the judge's reading.
- `outcome`: `right`/`wrong` for a run, `good`/`bad`/`unclear`/`open` for a session. `by` says who said so, surest
  first: `pr` (what came of its pull requests), `person` (their rating, or their next message grading the request),
  `reward` (a reward given), `judge` (a model's reading).
- Each trail lists the caller's own runs and sessions, newest first, `limit` (12, at most 200) a side, `more` the rest;
  other people's are counted in the totals and `hidden`, never listed. An org's admins, reading the org's own memory
  where the org shows its people's activity (as `notes/` in `/browse`), read every person's own lessons and every
  entry in full; `person` narrows to one person's own (a member may name only themselves, else 400).
- `since` (µs, an RFC 3339 time or a date) keeps the lessons written at or after it; `key` reads one (404 if the
  caller may read no such lesson).

Every read is filtered by what the caller may see. A path or id that holds nothing the caller may see answers 404,
as if it didn't exist.

## Writing

| Route | Who | Body |
|---|---|---|
| `POST /ingest` | an org's `ingest` key, or the operator | `{episodes: [{source, occurred_at (µs), acl, about?, index_text?, document?, replaces_source?, body}]}`, 1 to 500, up to 8 MiB, one commit |
| `POST /facts` | a caller who may read and write | `{facts: [{entity, predicate, value, quote, source_episodes, valid_from?}]}`, 1 to 500 |
| `POST /close` | a caller who may write | `{fact_id, reason, when?, acl?}` |
| `POST /forget` | the note's author | `{episode_id}` |
| `POST /retract` | the operator, an org admin, or an ingest key for its own records | `{source_prefix \| source \| episode_id, except?}` |
| `POST /acl` | admin key | `{source_prefix \| source \| episode_id, acl}` |
| `POST /feedback` | a person who may read the fact | `{fact_id, signal: positive \| negative \| very_negative, reason?}` |

- `/ingest`: an ACL must name its readers (empty or `*` is a 400). A record for a whole org names
  `group:<org>.all`. Every episode must be of a known [episode type](concepts.md#episodes); one bad record refuses
  the batch. `replaces_source: true` retires the source's other versions once the batch commits.
- `/facts`: each fact's quote (at least 12 characters) must occur in a cited episode, and every word of its entity
  and value (common words aside) must occur in the cited text.
- `/forget` re-ACLs the episode to an entry nobody holds; the ledger keeps the change.
- `/retract` hides an episode, and every fact citing it, for every reader on every branch at every time. The bytes
  stay until a purge (`ampup-memory org purge`).

## Admin and operations

| Route | What it does |
|---|---|
| `POST /admin` | Directory and settings ops as JSON: `create_team`, `invite`, `set_settings`, `get_schema`, `set_schema`, `learn_advice`, `audit`, `retention`, `forget_user`, `export_org`, `snapshot`, `check_directory`, …; `"dry_run": true` where it applies |
| `POST /closure` | Push a user's group changes or disable them (admin key) |
| `POST /team`, `DELETE /team` | Replace or clear a team csv on every server (admin key) |
| `POST /installations` | Register which org owns a connector installation (admin key) |
| `POST /digest/preview` | A person's weekly digest, sending nothing: `{person?, week?}` (`last` by default); the caller's own, or an org admin's for a member |
| `GET /digest/unsubscribe?t=…`, `POST /digest/unsubscribe` | A digest's unsubscribe link: the page asks; a POST (`t=…`, `subscribe=on` to subscribe again) changes it, one-click (RFC 8058) too |
| `GET /metrics` | Prometheus text; the admin key only on the public port, or anyone on `AMPUP_MEMORY_METRICS_PORT` |
| `GET /.well-known/oauth-protected-resource` | The identity provider and CLI client id, for OAuth sign-in (RFC 9728) |

The CLI's people commands (`ampup-memory org`, `team`, `member`, `apikey`, `audit`) call `/admin` when signed in. See
[security-model.md](security-model.md).

## Errors

- 400 for a malformed body, with a reason.
- 401 without a credential the server accepts, answered before the body is read; 403 when the caller may not do this here, with a reason code
  (`not_member`, `as_denied`, …).
- 404 for anything the caller may not see.
- 429 with `Retry-After` past the caller's rate limit (a batch costs a token per item) or past its requests in
  flight at once (`AMPUP_MEMORY_MAX_IN_FLIGHT_PER_CALLER`).
- 503 for advice with no routing model, from `/ready` during warm-up, and with `Retry-After: 1` when the server is
  at its connection or in-flight cap.
