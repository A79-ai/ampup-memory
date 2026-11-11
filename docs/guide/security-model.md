# Security model

## Principles

- **Your catalog, your storage.** Memory is Iceberg tables in a catalog and bucket you run. The search index is
  derived from them and rebuildable.
- **Isolation by namespace.** Each org, team and user scope is its own namespace. A team's memory is isolated from
  the rest of its org by the catalog, not by a filter.
- **A fact is as private as its sources.** A reader sees a fact only if they may read every episode it cites.
  Summaries, skills, notes and dream decisions follow the same rule: they are readable by exactly the audience of
  what they were made from.
- **Text is data.** Every prompt fences source text off with a line derived from the text's own SHA-256, and says
  to ignore what the text asks. Facts must quote their source verbatim.

## Who a caller is

`AMPUP_MEMORY_CALLER_IDENTITY` picks how callers are identified. It takes a comma-separated list; the first source
whose credential a request carries decides, and a credential that source refuses is refused, never tried on the
next.

| Value | Callers |
|---|---|
| unset | No identity: every request reads as one principal with the groups `serve` was given. `serve --local` works this way and binds to 127.0.0.1 only. |
| `token` | API keys (`msk_…`), signed JWTs, and people's ID tokens from an OIDC provider (`AMPUP_MEMORY_OIDC_ISSUER`, `AMPUP_MEMORY_OIDC_AUDIENCE`). |
| `google` | Google ID tokens behind Cloud Run IAM, or people through Identity-Aware Proxy (`AMPUP_MEMORY_IAP_AUDIENCE`). |
| `snowflake` | The Snowflake user who made the request, inside Snowpark Container Services. |

JWTs: keys from `AMPUP_MEMORY_JWT_JWKS_URL` (RS256 by `kid`, refetched on rotation), `AMPUP_MEMORY_JWT_PUBLIC_KEY` (RS256
PEM) or `AMPUP_MEMORY_JWT_SECRET` (HS256). The algorithm comes from the key source, never the token. Claims are
configurable (`AMPUP_MEMORY_JWT_ISSUER`, `_AUDIENCE`, `_ORG_CLAIM`, `_GROUPS_CLAIM`, `_USER_CLAIMS`). A token whose
`email_verified` is false is refused.

Emails compare case-insensitively everywhere; other ids are exact.

With `token` identity, set `AMPUP_MEMORY_PUBLIC_URL` to the server's public URL: sign-in discovery and every 401's
`WWW-Authenticate` name it, never a request's `Host` header (which the caller sets). A server that listens beyond
loopback refuses to start without it.

`ampup-memory identity check` reports what `serve` makes of the environment and exits non-zero when a part fails. Run
it in a deploy's smoke step.

## ACLs and groups

An episode's ACL is a short flat list: `*`, `user:<id>`, `group:<id>`, `role:<name>`. A caller's groups (teams,
roles, accounts, channels) are flattened by your systems. Visibility is a roaring bitmap per ACL entry over episode
ordinals, so a membership change rewrites one caller's groups and no episode.

- `POST /acl` (admin key) changes the ACL of every episode under a source, a source prefix or an id: the change feed
  of a source (a Slack channel going private). Readers it leaves out lose those episodes, and the facts citing
  them, on their next read.
- `POST /closure` (admin key) pushes a user's group changes, or disables them (`"disabled": true`), on every server
  sharing the catalog within a second.

## Orgs, teams and people

One catalog holds orgs; each org holds teams. People belong to several orgs and teams and **select** which memory
they read. The server checks the selection against their memberships on every request.

```bash
ampup-memory create-org acme --admin alice@acme.com
ampup-memory join https://memory.example.com              # Alice signs in
ampup-memory org init                                     # a memory profile: what acme does, what never to keep
ampup-memory create-team sales
ampup-memory invite --email bob@acme.com --team sales     # or --file people.csv
```

- `invite` sends no email: it prints the `ampup-memory join` and `claude mcp add` lines to share.
- `org sync people.csv [--dry-run] [--keep-absent]` makes the org match an IdP export.
- An org can connect its own identity provider (`ampup-memory auth setup --provider google|okta|entra|oidc …`,
  `--require` to make it the only sign-in that counts there: not the server's provider, a gateway, an app's JWT or a
  keys-file key; services use the org's own keys), map provider groups to teams (`auth map-group`) and admit verified email
  domains (`auth domain add`). A sign-in through an org's own provider counts in that org only.
- Every URL the server fetches for an org's provider must be https and resolve to public addresses only; loopback,
  private, link-local (including the cloud metadata service) and CGNAT ranges are refused, and the connection is
  pinned to the checked address.
- A provider's client secret is sealed with `AMPUP_MEMORY_SECRET_KEY` (AES-256-GCM, 32 bytes) before it reaches the
  directory. Keep the same key on every server that shares a catalog.

Every directory change is a row in `memory_system.directory` with who made it and when. Changes commit by
compare-and-set; of two racing creates of one name, exactly one succeeds.

## Service accounts (API keys)

```bash
ampup-memory apikey create --name dash --team sales                    # a reader of acme/sales
ampup-memory apikey create --name feed --role ingest --sources slack:   # writes Slack episodes only
ampup-memory apikey create --name ci --role recorder                    # records runs as a member it names
ampup-memory apikey list
ampup-memory apikey revoke dash
```

| Role | May |
|---|---|
| `reader` (default) | Read its org's (or team's) memory. |
| `writer` | Also `remember_memory`. |
| `ingest` | Only write: `POST /ingest`, `remember_memory`, and retract its own records. Refused on every read. May be held to source prefixes. |
| `directory` | Manage the org's members and teams (a SCIM bridge). Touches no memory; can't promote or remove admins. |
| `recorder` | Record runs, rewards, advice and sessions **as a member** named in `x-memstore-as`, on those routes only. |

- The key (`msk_` and 256 random bits) is made on your machine and printed once. The server stores only its
  SHA-256.
- A key reads the memory everyone in its scope may read; it holds no groups.
- A revoked key is refused within 5 seconds on every server; an expired one at once.
- Give each connector its own key with the least role it needs.
- `x-memstore-as` is refused (403) on every route but `/runs`, `/runs/reward`, `/advice`, `/sessions*` and `/mcp`,
  and from any caller but a recorder key. The named email must be a member the directory holds.

## Notes, sessions and sharing

- A person's notes, run traces and session transcripts are readable by them alone.
- What a user tells an agent reaches teammates only once at least three distinct people have said the same thing,
  as one sentence built from words all their notes share, naming no one ([mcp.md](mcp.md)).
- Starter memory (a repo's agent instructions) goes into the person's own memory, so a hostile checkout can't reach
  teammates.
- Advice learning reads every rewarded run in a scope, so only an org's admins start it. Anyone who may record runs
  can steer what a note says ([advice.md](advice.md#trust)).

## Audit log

```bash
ampup-memory audit --org acme                                  # last 24 hours
ampup-memory audit --org acme --since 7d --kind refusal --json # change | signin | refusal | key
```

It records access changes, sign-ins (first per hour per org), every 401 and 403 with a reason code, and per-key
reads and writes per hour. Only the org's admins (signed in as people) and the operator's admin key read it.
Nothing a caller types is stored: routes are templates, reasons are codes, ids are cleaned, and tokens, keys,
queries and memory are never written. Servers buffer events and append them every `AMPUP_MEMORY_AUDIT_FLUSH_SECS` (60)
and on shutdown.

## Deletion

```bash
ampup-memory org export acme --out ./acme-export      # org admins, or the operator
ampup-memory users forget carol@acme.com --org acme   # delete what she wrote, and remove her
ampup-memory org purge acme                           # delete from storage what was retracted or aged out
ampup-memory org delete acme --confirm acme           # the operator only; no undo
```

- **Forget** removes the person from the org and its teams, then deletes every episode they authored, every fact
  citing one, and their quotes and closures, by rewriting each table so nothing is left in any snapshot or file.
  The search indexes are dropped and rebuilt. A tombstone records who forgot whom and how much went, never what.
- **What forget can't find:** memory that doesn't name its author (episodes ingested from Slack, calls, email or
  a CRM). Those go with the source's own deletion (`POST /retract`) and a purge.
- **Purge** deletes from storage what `POST /retract` hid and what aged out under retention.
  `set_settings {purge: true}` has the nightly window do it once hidden for `AMPUP_MEMORY_PURGE_GRACE` (7 days).
- **Delete** drops every table of the org's namespaces with purge, then its rows in the ledger, audit and key-usage
  tables.
- Every step skips what is done, so a stopped forget or delete is finished by running it again.
- Before the first forget or delete, the server checks the catalog can rewrite and purge tables, and refuses with
  nothing changed if it can't (for example Apache Polaris without `ALLOW_TABLE_LOCATION_OVERLAP` and
  `DROP_WITH_PURGE_ENABLED`).
- The model-call cache, the embedding cache and backups keep what they hold after a forget. A deployment that must
  erase leaves the caches off, or clears them, and expires backups within the period it must.

## Retention

```bash
ampup-memory org retention acme                                        # show
ampup-memory org retention acme slack:=90d slack:C-legal:=forever =2y  # set (replaces the old policy)
ampup-memory org retention acme --team a1 --clear
```

An episode follows the rule with the longest matching source prefix and is kept when none matches. Age counts from
`occurred_at`. No rule keeps less than a day. An episode past its age is gone for every reader at once, with every
fact citing it; loosening the policy brings back what is still stored until a purge.

## Personal identifiers (opt-in)

With `pii: true` on an org or team, text written from then on has card numbers (Luhn-checked), US social security
numbers, IBANs (mod 97) and international phone numbers replaced by `[card]`, `[ssn]`, `[iban]`, `[phone]` before it
is stored. Emails and names are not touched. It is a safeguard against identifiers pasted as usually written, not
against someone hiding one on purpose.

## Rate limits

`AMPUP_MEMORY_RATE_LIMIT=60/min:120` (rate and burst) and `AMPUP_MEMORY_INGEST_RATE_LIMIT` give each caller a token bucket,
keyed by what authenticated them. Over the limit: 429 with `Retry-After`. Buckets live in each server's memory, so
with N replicas a caller gets up to N times the limit. The operator's admin key and unauthenticated callers (who get
no further than 401) aren't limited; put a per-address limit at the load balancer. A batch costs its items: a
`/search` or `/query` with 100 rows costs what 100 requests would.

A request's body is read only once its headers prove who sent it: without a credential, or with one the server
refuses, it is answered 401 unread. Past `AMPUP_MEMORY_MAX_CONNECTIONS` or `AMPUP_MEMORY_MAX_IN_FLIGHT` a request is
answered 503, and past `AMPUP_MEMORY_MAX_IN_FLIGHT_PER_CALLER` 429, both unread and with `Retry-After: 1`; `/ready` and
the health probes are always answered. `AMPUP_MEMORY_HEADER_TIMEOUT_SECS` closes connections slow to send their
headers ([configuration.md](configuration.md)).

## Operating rules

- Keep the `memory_system` namespace writable by the servers and the operator alone: whoever can write it can
  write directory changes.
- Directory changes carry a format version. A server that meets a newer one, or an op it doesn't know, stops
  folding the directory: it signs no one in, refuses directory changes, answers `/ready` with 503, logs
  `ALERT directory: …` and sets `memstore_directory_blocked` to 1. Upgrade readers before writers: ship the code
  that reads a new change version before the code that writes it.
- Directory snapshots are sealed with `AMPUP_MEMORY_SECRET_KEY`. A server starts from a snapshot without reading the
  log before it only if its own key sealed it; any other snapshot is checked against the log, and one that doesn't
  match is refused with an alert (`memstore_directory_snapshot_refused_total`).
- `/metrics` labels come from fixed sets the server owns; no user, key, token or raw path is ever a label.
- Known gaps: Entra's groups overage (a token over its group limit carries none) and Google ID tokens carry no
  groups, so those callers get none.
