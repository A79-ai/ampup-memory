# The directory: design and threat model

The directory says who belongs to which org and team, which keys exist, and which identity provider each
org uses. This document is the reference for changing it. Read it before writing code that touches the
directory or authentication.

Code is cited as `file:function`, because line numbers go stale.

## Tables

| Table (`memory_system.`) | What it holds | Written by | Read by |
|---|---|---|---|
| `directory` | The log: one row per change (`at, actor, op, org, team, user, role, detail, version`). Rows are only ever appended. | `Store::append_directory_with` (a CAS commit) | `Store::directory` (the fold), `Store::directory_events` (the whole log: audit, checks) |
| `directory_snapshot` | The log folded: `at, events, digest, format, state, taken_at, actor` | `Store::directory_written` (every 500 changes, `MEMSTORE_DIRECTORY_SNAPSHOT_EVERY`), `/admin {"op":"snapshot"}` | `Store::fold_directory_from`, on a server's first load only |
| `key_usage` | When each key was last used: `sha256, org, name, at`. Kept apart from the log so the log stays small. | `server::flush_key_uses`, at most once per key per hour, through `Store::append_key_usage`, which writes no use of a key the directory (read then) no longer has in its org | `Directory::with_usage` (`show`, `apikey list`) |
| `audit` | Sign-ins, refusals, key use, each stamped with `org` and `org_created_at` (the incarnation) | `audit::Audit::flush` (every 60 s, `MEMSTORE_AUDIT_FLUSH_SECS`), through `Store::append_audit`, which writes no row of an org the directory (read then) no longer has as that incarnation (`audit::row_stays`) | `audit::collect`, `audit::collect_operator` |

## 1. The model

### Events

Each change is planned into events by `directory::plan`, and the events are folded by
`Directory::apply_events`. Every event is at version 1 (`EVENT_VERSION = 1`); a row from before
versions also counts as 1. The `role` and `team` columns are set on `key.create` too, but the fold reads
only `detail` for keys.

| op | Fields used | Fold effect | Written by (`Change` / function) |
|---|---|---|---|
| `org.create` | org | Adds the org if absent, with `created_at = at` (its incarnation). Removes the name from `purged`. | `CreateOrg` |
| `org.delete` | org | Removes the org. Adds the name to `purging`. Its verified-import keys go into `retired_keys`. | `DeleteOrg` |
| `org.purged` | org | Moves the name from `purging` to `purged`. | `directory::apply`, after `offboard::purge_org` |
| `team.create` | org, team | Adds an empty team. | `CreateTeam`, `Sync` (for teams it names) |
| `team.delete` | org, team | Removes the team and its keys. Verified imports among them are retired. | `DeleteTeam` |
| `member.add` | org, user, role | Sets the role (`admin` or `member`; anything else counts as `member`). Clears `removed`. | `CreateOrg` (first admin), `AddMember`, `AddToTeam`, `Invite`, `Sync` |
| `member.remove` | org, user | Removes the membership and every team membership. Adds the user to `removed`, so a domain or a group won't bring them back. The planner writes a `key.revoke` with it for every key imported in the user's name there (`directory::import_revokes`). | `RemoveMember`, `Sync`, `offboard::forget_user` |
| `member.forget` | org, user, detail = `Forgotten` counts | None. It is a tombstone, used by `describe` and the audit. | `offboard::forget_user` |
| `member.request` | org, user | Adds the user to `pending` (with `at`), unless they are a member. Grants nothing: `access` never reads `pending`. `member.add` and `member.remove` clear it. | `RequestJoin` (the person themselves, through `/admin` op `request_join`: `memstore join`) |
| `member.decline` | org, user | Removes the user from `pending`. | `Decline` |
| `team.add` | org, team, user | Adds the user to the team, but only if they are a member and the team exists. | `AddToTeam`, `Invite`, `Sync` |
| `team.remove` | org, team, user | Removes the user from the team. The planner writes a `key.revoke` with it for every key of that team imported in the user's name. | `RemoveFromTeam`, `Sync` |
| `org.profile` | org, detail = `Profile` JSON | Sets the profile. JSON that doesn't parse clears it. | `SetProfile` |
| `auth.provider` | org, detail = `Provider` JSON or none | Sets the org's IdP. With no detail, it removes it. | `SetProvider`, `RemoveProvider` |
| `org.auth0` | org, detail = Auth0 Organization id (`org_…`) or none | Sets the org's `auth0_org`. With no detail, it unbinds it. | `CreateOrg` (with `auth0_org`), `SetAuth0Org` |
| `domain.add` / `domain.remove` | org, detail = domain, team | Adds the domain (with its join team) or removes it. | `AddDomain` / `RemoveDomain` |
| `group.map` / `group.unmap` | org, team, detail = group | Adds or removes the group's link to the team. | `MapGroup` / `UnmapGroup` |
| `key.create` | org, detail = `KeyDetail {name, sha256, team, role, expires_at, sources?, created_by, imported_from?, verified?, imported_user?}` (version 2 when it has `sources`) | Inserts the key by name, replacing any key with that name. A verified import (`imported_from` and `verified`) removes its sha256 from `retired_keys`. An import keeps the user it acted as (`imported_user`, or on older rows the user `imported_from` names). A detail that doesn't parse (an unknown role, say) is skipped. | `CreateKey`, `ImportKeys` |
| `key.revoke` | org, detail = key name | Removes the key. A verified import goes into `retired_keys`. | `RevokeKey` |
| `key.sources` (version 2) | org, detail = `KeySources {name, sources}` | Sets the key's source prefixes (empty: any source). A key not there is skipped. | `SetKeySources` |
| anything else | | Ignored | |

The fold also keeps state that belongs to no single org: `retired_keys`, `purging`, `purged`, and each
org's `removed` and `created_at`. Snapshots keep all of it (`SnapOrg`, `SnapKey`,
`Directory::snapshot_json`). `SNAPSHOT_FORMAT` is 9 (an imported key's user is kept since 9).

### Invariants and where each is enforced

`plan` runs inside the CAS (`Store::append_directory_with`), so each check below is made against the
directory as it is when the change commits. The fold does not re-check them.

| Invariant | Enforced in |
|---|---|
| Org, team and key names match `[a-z0-9_]`, with single underscores, at most 48 characters | `lib.rs:check_name`, called from `plan` |
| Org names are unique, and a name can't be re-created while it is `purging` | `plan` (`CreateOrg`) |
| Team names are unique within an org. Key names are unique within an org. | `plan` (`CreateTeam`, `CreateKey`, `ImportKeys`) |
| **One identity per key.** A sha256 names at most one directory key across all orgs. | `plan` (`CreateKey`: "already in use"; `ImportKeys`: "under another name") |
| At authentication, a hash that matches two keys names neither. Keys are compared in constant time. | `tokens.rs:org_key_caller` |
| A new key is none of the file keys known where it is made: the server's keys file, or with `--direct` the catalog owner's `MEMSTORE_API_KEYS` (unreadable: the change is refused). A bearer that is a non-admin file key on a server and also a directory key other than its verified import (made where the keys file didn't hold it) names two callers, so that server refuses it: one bearer is never one caller here and another elsewhere. | `plan` (`CreateKey`, `Actor::file_keys`); `cli.rs:direct_file_keys`; `server/mod.rs:request_principal` |
| The server hashes a new key. A client never names a hash. | `Change::CreateKey { key: NewKey }`; `directory_change` refuses a `sha256` field |
| A retired hash can't be created again. Only a verified re-import brings it back. | `plan` (`CreateKey`); `apply_events` (`key.create`) |
| An admin file key is always the file's. A non-admin file key is replaced by its verified import, and by no other directory key. A retired one is refused, never served as its file twin. | `server/mod.rs:request_principal` (the `Identity::Token` arm) |
| Imports are verified: the server holds the key, or the catalog owner runs the import. The row must match the key held under that sha256 (id, user, groups, admin). | `plan` (`ImportKeys`: `actor.owner \|\| actor.vouched_keys`); `directory_change` (the row match) |
| Only the operator or the key's holder may import. Imports skip the org-admin gate. | `plan` (the import gate at the top) |
| An import gets no more than it had: no admin rows, no groups, and the user must be able to reach the scope (never an org that requires its own provider, which file keys don't reach). It keeps no more than its user has: removing the user from the org, or from the key's team, or forgetting them, revokes the key in the same commit (which retires its hash, so the file twin is refused too), and a key whose user is no longer a member there reads nothing, whichever planner removed them. | `plan` (`ImportKeys`, `import_revokes`); `offboard::forget_user`; `Directory::access` |
| A key reads its own scope only. A team key never gets its org's scope. A key is served only while the fold still has it under that hash. | `Directory::access`, `Directory::resolve` |
| `reader` never writes, and `ingest` never reads | `SignIn::may_write`, `SignIn::may_read`; the route gates in `server/mod.rs:handle` |
| A `directory` key is org-wide, never imported, reads and writes no memory, and changes only its own org's non-admin members and its teams. It deletes no team holding admin-made config (team keys, team settings, group mappings, domains joining people to it) | `plan` (`CreateKey`, `ImportKeys`, `directory_key_may`); `directory_change` (`DIRECTORY_KEY_OPS`); `server/mod.rs:handle` (every other route); `Directory::resolve` |
| An org's own IdP vouches for that org only. An org with a `required` provider takes no other sign-in, a person's or a service's: not the server's IdP, Auth0, a gateway (IAP, a Google ID token, Snowflake), an app JWT or a file key. Only its own provider's sign-ins and its own directory keys (which its admins make) get in. | `Directory::vouched`, `Directory::access` (keys first); `tokens.rs:authenticate_any` (`orgs`) |
| Closure groups (`/closure`, the team csv) are pushed for a bare email, so they apply only to a sign-in whose email the deployment vouches for: the server's IdP, the Auth0 tenant's own connections, an app JWT, a gateway, a key. A sign-in through an org's own IdP (or an Auth0 Organization's enterprise connection), which says what the email is, gets none. The disabled list applies to every sign-in. | `server/mod.rs:request_principal` |
| User ids and IdP emails fold ASCII letters only, as `acl.rs` does: an address that only Unicode case rules fold into a member's (a Kelvin sign for `k`) is someone else. An id stored with other letters matches only that spelling. Forgetting a person also takes what was written under the Unicode-folded spelling of their id. | `directory.rs:user_id`, `forgets`; `oidc.rs:email_of`; `lib.rs:remembered_by` |
| An org's whole view (`show`: members, teams, keys, provider, domains, requests) is its admins' (signed in as a person, not a file key in an admin's name), its directory key's and the operator's. Any other member sees themselves: their role, their teams and the org's profile. | `server/mod.rs:directory_change` (`show`); `Org::member_view` |
| An IdP client (issuer and client id) belongs to one org. It is never the server's own client. | `plan` (`SetProvider`); `directory_change` |
| An Auth0 Organization belongs to one org. At authentication, one held by two orgs binds neither. The operators' Organization binds no org. Only the operator (an admin key, the catalog owner, or an Auth0 operator) binds or unbinds an org; never its admins. | `plan` (`owner_only`, `auth0_free`); `Directory::auth0_bound`; `directory_change` (the operators' Organization) |
| An Auth0 token counts only in the org bound to its `org_id`, as both the 5 s view (`DirectoryAuth::auth0_orgs`, `via.orgs`) and the 1 s fold say, never in an org that requires its own provider, and only for a member (by the token's verified email; an org's domains admit no one through Auth0). No `org_id`, through one of the platform's own connections (Google, the tenant's database, passwordless): the directory's member wherever it lists them, like an MCP client's access token (`memstore join` signs in as the person, then asks which orgs are theirs). No `org_id` through an enterprise connection, or an `org_id` no org is bound to: signed in, bound to no org. The operators' Organization's tokens are bound to no org. | `tokens.rs:Auth0::caller`; `Directory::vouched` |
| The Auth0 tenant is its own checker: a token from its issuer for one of its client ids is checked by it alone. The tenant is never the server's own provider, and the app JWT's audience is never one of its client ids (start refused). An org's provider can't name the tenant's issuer, and one that does vouches for no one. | `TokenAuth::is_auth0`, `with_auth0`, `authenticate_any`; `directory_change` (`SetProvider`) |
| An Auth0 operator makes `CreateOrg`, `SetAuth0Org`, `AddMember`, `RemoveMember`, `Invite`, and `CreateKey` / `RevokeKey` of `ingest` keys, as an admin key would (any org, any member role, rebinding included), and reads every org's `show`; nothing else an admin key may do. An admin they make, themselves included, then holds the "own" column there. `/me.admin` says true for them; nothing else reads it. | `Change::operators` (from `plan`, inside the CAS: a revoked key's role is read there); `directory_change` (`show`); `SignIn::operator` |
| A client secret is never stored in plain text, and an `env:` secret names only its own org's variable | `plan` (`SetProvider`); `directory::seal_secrets`; `secret.rs:protect` |
| A provider with a client secret declares its client type. A confidential client's secret is never taken, and never served even if the directory holds one. | `directory::client_type_problem` (from `seal_secrets` and `plan`); `server/mod.rs:protected_resource`; `directory_secrets_check` |
| The server fetches from an org's provider only over https, from public addresses, pinned to the vetted address, with every redirect hop vetted. Failures read the same. | `auth/fetch.rs` (`FetchPolicy::Vetted`); `tokens.rs` (`check_issuer`, `org_issuer`) |
| A domain belongs to one org. Public mail domains are refused. An org admin may add only their own email's domain, proven through the server's IdP. | `plan` (`AddDomain`) |
| An admin can't remove their own admin role | `plan` (`keeps_me`) |
| Changes apply in commit order: each is stamped after the last change it was planned against | `append_directory_with` (`start = max(now, last_at + 1)`) |
| No commit is planned against a stale directory. A CAS conflict re-reads and re-plans the change, up to 50 times. A commit whose answer was lost is detected. | `append_directory_with`; `ensure_directory_cas` (Iceberg's automatic retry is set to 0) |
| A change of a newer version is never folded. Writes and snapshots are refused while the fold is blocked. | `Folded::apply`; `append_directory_with`; `snapshot_directory` |
| A snapshot is loaded only if its format is current, it is not dated in the future, and its event count matches the log. Its digest is not checked. | `Store::directory_snapshot`; `Store::fold_directory_from` |
| Audit rows belong to one incarnation of an org | `server/mod.rs:member_org` and `key_caller` (the stamp); `audit::collect` (the filter) |

### An org's lifecycle

```
(absent) --org.create--> live --org.delete--> purging --purge_org, then org.purged--> purged --org.create--> live (new created_at)
```

| State | What is refused or done | Where |
|---|---|---|
| live | Normal operation. `created_at` names this incarnation. | `apply_events` |
| purging | Nothing is served. Every key, provider and member is gone. `CreateOrg` is refused. Running `DeleteOrg` again finishes the purge. | `plan` (`CreateOrg`, `DeleteOrg`); `directory::apply` |
| purged | The memory is gone. A lagging replica may still have written to it. `CreateOrg` purges the namespaces again before creating. | `directory::apply` (`purge_memory`, before `append_directory_with`) |
| re-created | This is a new org. Old audit rows and old changes are hidden from it. | `audit::collect` (the incarnation filter) |

## 2. Who may write each event

Callers are the ones in the review's threat model. In the table, ✓ means allowed, ✗ means refused, and
"own" means within the caller's own org. A member column with ✗ also covers an outsider.

- **Admin** is an org admin who is a person: they signed in through the server's IdP, an app JWT, or a
  gateway such as IAP, Snowflake or a Google ID token. In an org that requires its own provider, only
  through that provider (the **Org IdP** column).
- **Org IdP** is what an org's own IdP can do. It controls the email claim, so it can sign in as any
  member or admin *of that org*.
- **Keys** are directory keys (`reader`, `writer`, `ingest`). They are all refused the same way, at the
  top of `directory_change`.
- **Dir key** is a `directory` key (a SCIM bridge's). `directory_change` lets it send `DIRECTORY_KEY_OPS`
  only, and `plan` (`directory_key_may`) holds it to its own org, to members who aren't admins, and to the
  key still being in the directory.
- **File key** is a non-admin key from `MEMSTORE_API_KEYS`.
- **Operator** is the admin file key, or the catalog owner using `--direct`. For a directory change,
  `superuser` is true only for these two.
- **Auth0 op** is a person signed in through the platform's Auth0 tenant (`MEMSTORE_AUTH0_ISSUER`) for
  the operators' Organization (`MEMSTORE_AUTH0_OPERATORS_ORG`): `SignIn::operator`, never `superuser`.
  A person signed in through Auth0 for an org's Organization is a Member or Admin of that org only.
- **Catalog write** is anyone with write access to `memory_system`. It bypasses every check, because it
  appends rows itself.

| Change (events) | Anon | Member | Admin | Org IdP | Keys | Dir key | File key | Operator | Auth0 op | Catalog write | Check lives in |
|---|---|---|---|---|---|---|---|---|---|---|---|
| `CreateOrg`, `DeleteOrg` (`org.create`, `org.delete`, `org.purged`) | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | ✓ | `CreateOrg` only | ✓ | `plan` (`owner_only`) |
| `SetAuth0Org`, `CreateOrg` with `auth0_org` (`org.auth0`) | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | ✓ | ✓ | ✓ | `plan` (`owner_only`, `Change::operators`, `auth0_free`); `directory_change` (not the operators' Organization) |
| `CreateTeam`, `DeleteTeam`, `AddMember` (role `member`), `RemoveMember`, `AddToTeam`, `RemoveFromTeam` | ✗ | ✗ | own | own | ✗ | own, never an admin (not adding, demoting or removing one); `DeleteTeam` only of a team with no keys, team settings, group mappings or domains | ✗ | ✓ | `AddMember`, `RemoveMember` only (any org) | ✓ | `plan` (`is_admin`, `directory_key_may`); `directory_change` (refuses other keys and file keys) |
| `AddMember` (role `admin`), `Invite`, `Sync`, `SetProfile` | ✗ | ✗ | own | own | ✗ | ✗ | ✗ | ✓ | `AddMember`, `Invite` only (any org) | ✓ | `plan` (`is_admin`, `directory_key_may`); `directory_change` (refuses keys and file keys) |
| `RequestJoin` (`member.request`) | ✗ | n/a (a member already: refused) | n/a | ✓ for themselves, with a verified email | ✗ | ✗ | ✗ | ✓ | ✗ | ✓ | `plan` (`asks`: a person with a verified email, for themselves only; never in a workspace); `directory_change` (refuses keys and file keys) |
| `Decline` (`member.decline`) | ✗ | ✗ | own | own | ✗ | ✗ | ✗ | ✓ | ✓ (any org) | ✓ | `plan` (`is_admin`, `Change::operators`) |
| Approval link (`server/join.rs`: `Invite` as the admin it was mailed to) | ✗ | ✗ | own, holding the link mailed to them (sealed under `MEMSTORE_SECRET_KEY`, 7 days, one request) | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | `join::approve` (the token opens, the request still waits, then `plan` as that admin: one removed since approves nothing) |
| `SetProvider`, `RemoveProvider` (`auth.provider`) | ✗ | ✗ | own | own (it can replace itself) | ✗ | ✗ | ✗ | ✓ | ✗ | ✓ | `plan`; `directory_change` (`check_issuer`, not the server's client) |
| `AddDomain` | ✗ | ✗ | own, for their own verified email domain, through the server's IdP only | ✗ | ✗ | ✗ | ✗ | ✓ (any domain) | ✗ | ✓ | `plan` (`proven`) |
| `RemoveDomain`, `MapGroup`, `UnmapGroup` | ✗ | ✗ | own | own | ✗ | ✗ | ✗ | ✓ | ✗ | ✓ | `plan` |
| `CreateKey`, `RevokeKey` (`key.create`, `key.revoke`) | ✗ | ✗ | own | own | ✗ | ✗ | ✗ | ✓ | `ingest` keys only (any org) | ✓ (any hash) | `plan`; `directory_change` |
| `SetKeySources` (`key.sources`): an ingest key's source prefixes | ✗ | ✗ | own | own | ✗ (not even on itself) | ✗ | ✗ | ✓ | `ingest` keys (any org) | ✓ | `plan` (`key_sources`); `directory_change` (refuses keys) |
| `ImportKeys` (`key.create` with `imported_from`) | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | itself only (`--self`), into scopes its user reaches | ✓ (verified: the server holds the key, or `--direct`) | ✗ | ✓ | `plan` (the import gate, `vouched_keys`); `directory_change` (`--self`, the row match) |
| `forget_user` (`member.remove`, `member.forget`) | ✗ | ✗ | own, as a person, not themselves | own | ✗ | ✗ | ✗ | ✓ (every org) | ✗ | ✓ | `offboard.rs:may_manage`, `forget_user`: checked again inside the `member.remove` CAS, against the directory read fresh before each scope's memory is deleted, and inside the `member.forget` CAS. A caller who loses the right partway is refused there: the person is out of the org, no scope is half-forgotten, and someone who may can run it again to finish. |
| `set_schema` (a ledger commit, not a directory event) | ✗ | ✗ | own | own | ✗ | ✗ | ✗ | ✓ | ✗ | ✓ | `directory_change` |
| Read `show` | ✗ | own: themselves only | own | own (as whoever it signs in) | ✗ | own | its user's orgs: themselves only | all | all | — | `directory_change` |
| Read the org's audit; `export_org` | ✗ | ✗ | own | own | ✗ | ✗ | ✗ | ✓ | ✗ | — | `directory_change`; `offboard.rs:may_manage` |
| Read the operator's audit; `snapshot`; `check_directory` | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | admin file key only for the operator's audit | ✗ | — | `directory_change` |
| `/ingest`, `/acl`, `/closure`, `/team`, `/metrics` (public port) | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | ✗ | admin file key | ✗ | — | `server/mod.rs:require_admin` |

## 3. Trust boundaries and assumptions

### Write access to the catalog is root

Anyone who can write `memory_system` is fully trusted. Nothing in the engine defends against them, and
this document does not try to. They can:
- append any directory event at version 1, which skips every check above, or at a higher version, which
  freezes every server;
- write a snapshot whose state doesn't match the log. It is trusted on first load if the event count
  matches. `check_directory` would catch it, but nothing runs that check on its own.
- rewrite the audit and key-usage tables, and memory.

So `memory_system` must be writable only by the servers and the operator. Read access to the bucket
exposes the directory: members, domains, key hashes and sealed secrets. That is why secrets are sealed
or referenced, and why key hashes are sha256 of keys with at least 32 random characters.

### What `MEMSTORE_SECRET_KEY` protects

| It protects | It does not protect |
|---|---|
| Org IdP client secrets stored as `sealed:…`, against anyone who reads the bucket. They are sealed with AES-256-GCM, with the org name as AAD, so a sealed value pasted into another org won't open. (`secret.rs:SecretKey::seal`, `open`) | The same secrets against anonymous callers: `protected_resource` hands out a public client's resolved secret, and an undeclared one's (which `identity check` flags). A confidential client's secret is never taken. |
| | `env:` secrets, which live in the server's environment |
| | The integrity of the log or of snapshots. Nothing is signed. |
| | Key hashes, key usage, the audit |

It must be the same on every replica, and on every machine that runs `--direct auth setup` (the CLI seals
with its own environment, in `cli.rs:Target::change`). A replica with a different key, or none, can't
open the secret. `protected_resource` then leaves it out (it logs the error and returns no secret), and
signing in to that org through that replica fails. `SetProvider` with a sealed value is refused there.
There is no key rotation: to rotate, change the key everywhere, then run `auth setup` again for each org.
`memstore identity check` (`directory_secrets_check`) reports secrets that don't open here.

### How far replicas lag

| What | Window | Where | On failure |
|---|---|---|---|
| Directory fold (memberships, `resolve`, `show`, `/me`, audit stamps) | 1 s (`MEMSTORE_MAX_STALENESS_MS`). A server's own writes are visible at once. | `Store::directory`, `directory_loaded_after` | Blocks on the catalog. An error fails the request closed. |
| Keys, providers, `retired_keys` (authentication) | 5 s (`PROVIDERS_TTL`) | `server/mod.rs:directory_auth` | Fails closed. `resolve` re-checks the key against the 1 s fold, so a revoked key usually loses its scope within about 1 s. |
| Auth0 bindings (`auth0_orgs`) | 5 s for `via.orgs`, re-checked against the 1 s fold by `vouched`: a token counts only where both agree | `directory_auth`; `Directory::vouched` | Fails closed: a rebinding takes effect within about 1 s, an unbinding at once on the server that made it |
| Auth0 operators | The token's life (Auth0's ID token expiry, 10 h by default): removing someone from the operators' Organization takes effect when their token expires. The disabled list (`/closure`) stops them at once. | `tokens.rs:Auth0::caller` | — |
| Callers through a gateway (Google, Snowflake, IAP groups) | 110 s (`CALLER_TTL`). Not cached for bearer tokens. A closure push invalidates it. | `request_principal` | — |
| Memory tables | 1 s, served stale in the background up to 60 s (`HARD_STALENESS`) | `Store::rows` | — |
| Org IdP JWKS | 1 h TTL, refetched at most once every 30 s on an unknown `kid`. A failed issuer is retried after 60 s. | `tokens.rs` (`JWKS_TTL`, `JWKS_MIN_REFETCH`, `ISSUER_RETRY`) | — |
| The keys file (`MEMSTORE_API_KEYS`) | Looked at every 2 s (`KEYS_FILE_POLL`) and read again when it changes. The new keys take effect whole, so a key taken out is refused within about 2 s, with no restart. | `TokenAuth::reload_keys_file`, `server::keys_file_watch` | A file that can't be read or doesn't parse keeps the keys last read; `memstore_keys_file_failing` is 1 and the log says `ALERT` until it reads again |
| Audit buffer | 60 s. Refusals hold half of it; the people one org's IdP signed in hold at most 100 rows of their own, the rest counted in one row under the IdP (`idp:<org>`), so its minted names can't push out anyone else's. | `audit::flush_interval`, `Recorder::refusal_via` | Retried. An org delete flushes the deleting server's buffer before its purge. Another server's buffer may still hold the deleted org's rows: its flush drops them (`audit::row_stays`, against the directory read at the flush), and after writing it reads the directory again and removes any rows of an org deleted meanwhile (`Store::unwrite_gone`), so a delete that raced the write leaves none of them stored. |
| Key usage | 1 h | `KEY_USE_FLUSH` | Retried every minute. A deleted org's key uses are dropped and removed again as the audit's are (`audit::key_use_stays`). |
| A blocked fold (a newer version) | Until the server is upgraded | `Folded::apply` | Serves the directory as of the change before. Refuses writes. |

### Read paths and what they trust

| Path | Reads | Trusts |
|---|---|---|
| Bearer to caller (`request_principal`) | The keys file, `directory_auth` (5 s), and OIDC or JWT signatures, from the headers alone: `handle` runs it before reading the body, and refuses a caller it doesn't know with the body unread (BigQuery's service agent, which names its caller in the body, has its body read once its token is shown to be the agent's) | The catalog for keys and providers; the IdPs for identity, bound to their org; the app JWT issuer and gateways completely |
| Scope selection (`select_scope` → `Directory::resolve`) | The 1 s fold | The catalog. A caller with no selection and no directory route gets the served scope. |
| Memory reads (`/search`, `/query`, `/episode`, MCP) | The scope, then episode ACLs (`acl.rs`), then `may_read` | The catalog grants on namespaces. Facts are visible only if every cited episode is. |
| `/me`, `show` | The fold, plus `key_usage` for `show` | The catalog. `/me` is reduced for ingest keys, and refused to directory keys. |
| Audit (`audit::collect`) | The audit table, the **raw log** (to find the incarnation), and this server's buffer | The catalog, and the incarnation stamps |
| `protected_resource` (anonymous) | The fold; it resolves the secret of a client that isn't confidential. The `resource` it names, and every 401's `WWW-Authenticate`, come from `MEMSTORE_PUBLIC_URL` (`public_url`, at start) | Nothing about the caller, its `Host` header included |
| A server's first load | The newest snapshot plus the log after it | The snapshot, if its count matches |
| `check_directory`, `identity check` | A replay of the whole log, compared with the snapshot | The log |

## 4. Deploy and upgrade rules

| Rule | Why | Enforced by |
|---|---|---|
| **Upgrade readers before writers.** A release that writes a new event version reaches every server, and every `--direct` CLI, before any of them writes it. | An old reader freezes at the first newer row (fails closed). It never misreads it. | `Folded::apply`, the `memstore_directory_blocked` metric |
| Bump `EVENT_VERSION` when an event gains a field or meaning an old fold would ignore, or for a new op whose absence would widen access | Unknown ops and fields at a known version are silently skipped | Review only |
| Bump `SNAPSHOT_FORMAT` whenever `apply_events`, `SnapOrg` or `SnapKey` change | An old snapshot would load as the wrong state | Review only. A mismatched format means the log is replayed. |
| A new plan rule that depends on new fold state can't be protected by the version gate. Old binaries keep planning without it. Ship it with a version bump on the event the old planners would mis-handle, or stop old CLIs some other way. | The gate stops old *folds*, not old *planners* | — |
| Wire: `create_key` sends the key itself, `{"key": "msk_…"}`, and the server hashes it. A request with `sha256` is refused. **Upgrade servers before CLIs.** A CLI from before this change can't create keys against a server from after it, and the reverse fails too (the change doesn't parse). | A client-named hash allowed key takeover | `Change::CreateKey`, `directory_change` |
| Wire and fold: the `ingest` role. A server from before it can't parse a `KeyDetail` with `role: ingest`. It skips the `key.create`, so the key is unknown there, which fails closed. That is serde's doing, not the version gate's. **Upgrade servers before creating ingest keys.** | — | `apply_events` (`.ok()` on the detail) |
| Wire and fold: the `directory` role. As with `ingest`, an older server can't parse a `KeyDetail` with `role: directory`, skips the `key.create`, and so refuses the key (fails closed). **Upgrade servers before creating directory keys.** A snapshot an older server takes leaves the key out; a newer server that loads it doesn't know the key until it is made again. | — | `apply_events` (`.ok()` on the detail) |
| Fold: an ingest key's source prefixes (`key.create` with `sources`, `key.sources`) are written at version 2, every other change at 1. A server before them stops folding at the first such change (fails closed) instead of reading the key as free to write any source. **Upgrade servers before setting a key's sources.** | An old fold would drop the prefixes | `SOURCES_VERSION`, `Folded::apply` |
| `MEMSTORE_SECRET_KEY` is the same on every replica and on every `--direct` machine | See §3 | `identity check` (per replica) |
| `MEMSTORE_PUBLIC_URL` is set on every server with token auth that listens beyond loopback | Sign-in discovery names it; without it the only other source would be the request's `Host`, which the caller sets | `public_url` (`serve` refuses to start) |
| `MEMSTORE_API_KEYS` is the same on every replica | Each server serves its own file's keys, and the import row match is made against it. A file that differs fails closed (a bearer that is a directory key too is refused), but a key one file lacks works only where a file has it | Nothing; each server reads its file again when it changes |
| Never roll back past the release that added the event version gate | Binaries before the version gate fold any row. Binaries before `retired_keys` and `purging` don't know them. | — |
| New columns are added in place, and old rows read them as null (`org_created_at`, `version`) | Mixed fleets keep writing | `audit::collect` falls back to time for rows with no incarnation |

## 5. Threat model for future changes (paste into agent and PR prompts)

> Before writing code that changes the directory, a key, a role, an identity source or a read path,
> answer each of these in the PR description. Use `docs/directory.md`.
>
> 1. **Which callers** (from the matrix in `docs/directory.md` §2) can now do something they couldn't
>    before? Which can no longer? Fill in the new row or column of the matrix.
> 2. **Where is the check**, and does it run inside the CAS (`plan`, or a closure passed to
>    `append_directory_with`)? A check made outside it is a TOCTOU.
> 3. **Identity.** Can one bearer, hash, email or client now name two callers, in two orgs, on two
>    replicas, or before and after a revoke? Can a caller choose a value (a hash, an issuer, an env name,
>    an id) that someone else holds?
> 4. **The org IdP.** An org's own IdP controls the email claim. Does the change let it reach anything
>    outside its org, or rights tied to a bare user id (closures, the disabled list, file keys)?
> 5. **Keys.** Does any key role, file key or ingest key gain a read, a write or a management op? Does
>    revoking still stop it within the TTL, on every replica, including its file twin?
> 6. **The fold.** Is there a new op, field or meaning? Then: what does a server one release older do
>    with the row? If it would widen access or misread the row, bump `EVENT_VERSION`. If `apply_events`
>    changes, bump `SNAPSHOT_FORMAT`. If old *planners* must stop, say how.
> 7. **Lifecycle.** What happens to the new state on `org.delete`, `team.delete`, `member.remove`,
>    `key.revoke`, and when the name is re-created? Is it purged, retired or incarnation-stamped?
> 8. **Lag.** What does a replica that is 1 s, 5 s or 60 s behind do with it? What does a replica on the
>    previous release do?
> 9. **Secrets.** Does anything new reach the log, the audit, `/metrics`, errors or views? Does anything
>    new depend on `MEMSTORE_SECRET_KEY` or the keys file being the same everywhere?
> 10. **Wire.** Do older CLIs or newer servers break, or silently mean something else? State the deploy
>     order.
> 11. **Tests.** Which flow in `tests/` covers it? What is the refused-caller step? Show that it fails
>     without the fix.

## 6. Simplification candidates (not implemented)

| Candidate | Removes | Cost | Benefit |
|---|---|---|---|
| Retire non-admin file keys once operator keys live in the directory: import every one, then stop reading them | `SignIn::file_key`, `replaces_file_key`, `verified_import`, `retired_keys`, `KeyDetail.verified` and `imported_from` on new rows, `--self`, `vouched_keys`, the row match, the file-key op allow-list, `key_caller`'s `file:` branch | Medium. A migration (`apikey import` per deployment), then one release that still reads legacy rows | Removes a class of key-handling bugs, and the file keys that differ per replica. One key system in fact. |
| Model the caller as one enum (`Operator`, `Person{via}`, `OrgKey`, `FileKey`) instead of `superuser`, `owner`, `via.key`, `via.file_key` and `via.person`, and make `plan` the only gate | Gates split across `directory_change`, `offboard::may_manage` and `plan` | Medium refactor; the tests stay as they are | §2 becomes one `match`. A new op can't forget a caller kind. |
| Treat an unknown op at a known version as blocking, like a newer version | The silent `_ => {}` in `apply_events` | Small, but it needs a version bump itself | Fails closed when someone forgets the bump |
| Take the audit incarnation from the fold (`Org::created_at`), not from the raw log | `audit::collect`'s full log scan | Small | One source of truth, and the audit stops reading the whole log on every call |
| One cache for authentication: index the 1 s fold by sha256, and drop `directory_auth`'s separate 5 s cache | `PROVIDERS_TTL`, `DirectoryAuth` | Small to medium. Authentication then reads the catalog once a second, as `resolve` already does. | Revocation latency becomes one number, 1 s |
| Fold `purging` and `purged` into one `OrgState` per name, kept with the deleted incarnation | Two sets plus `created_at` handling | Small, plus `SNAPSHOT_FORMAT` 4 | The lifecycle in §1 becomes a real state machine |
| Derive `key_usage` from the audit's hourly `Key` rows | The `key_usage` table, its flusher, its purge | Small to medium. `apikey list` reads the audit instead. | One table fewer to purge on org delete |
| Verify the snapshot's digest on first load, or sign snapshots | Unchecked snapshot digests on first load | Small: one replay at startup, or an HMAC with a new secret | Snapshot tampering is detected |
