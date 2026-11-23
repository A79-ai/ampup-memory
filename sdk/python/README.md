# ampup-memory

A thin Python client for an [AmpUp Memory](https://github.com/A79-ai/ampup-memory) server: write what your team
knows, search it, and get advice that improves as sessions are rewarded. One class, no codegen, and its only
dependency is `httpx`.

```sh
pip install ampup-memory
```

## Quickstart

```python
from datetime import datetime, timezone
from ampup_memory import AmpUpMemory, micros

mem = AmpUpMemory.from_profile()  # the CLI's current context; or AmpUpMemory.from_profile("work")
# mem = AmpUpMemory("https://memory.example.com", "msk_...", org="acme")  # an API key: yours (apikey create --personal) or a service's
# mem = AmpUpMemory("http://127.0.0.1:8080", None)  # a local `ampup-memory serve --local`

ep = mem.remember("note:acme", "The Acme renewal is owned by Priya.")
print(mem.episode(ep["episode_id"]))
print(mem.search("who owns the Acme renewal", k=5))
print(mem.query({"find": "passages", "text": "Acme renewal"})["results"])
mem.ingest([{"source": "email:1", "body": "...", "occurred_at": micros(datetime.now(timezone.utc)), "acl": ["group:sales"]}])
```

## The learning loop

A session per task: advice as it goes, and how it went at the end. Advice is learned from sessions that ended with
a reward, and served to the next session of that kind of task.

```python
run = mem.session("Handle a refund request for order 8841", harness="support-bot")
print(run.opening["advice"])  # what the memory has learned for this kind of task, or None
print(run.advice("Issue the $620 refund", moment="next")["advice"])  # moments: next, fork, stuck, done
run.end("refund issued after sign-off", reward=1)  # -1 (failed) to 1 (succeeded)

mem.reward(run.id, -1, "the customer disputed it")  # or reward it later, once you know
```

Advice needs a server with a routing model: a hosted one, or `serve --local` with a model key (outside a session,
`advice()` raises a 503 without one). A `harness` ending in `:sdk` marks a session a program's: advice isn't learned
from it.

`from_profile()` uses the context and sign-in that `ampup-memory-client join <server> --org <org>` saved, in
`AMPUP_MEMORY_CONFIG_DIR` (default `~/.ampup-memory`). It picks the context from its argument, then
`AMPUP_MEMORY_CONTEXT`, then the current one, and renews an expiring sign-in with the saved refresh token, writing
it back where the CLI reads it.

## Serving many tenants (platforms)

A platform (an app that serves many customers of its own) uses one platform key for all of them. Each request names
the tenant, by the platform's own id for that customer, and, optionally, the user acting in it. Memory creates a
tenant the first time a request names it and enrolls a user the same way; each tenant's memory, and what it learns,
stays inside it.

```python
platform = AmpUpMemory("https://memory.example.com", "msk_...")  # the platform key, on your server only

# Per request: a copy for that tenant and user. Copies share the connections; close only `platform`.
mem = platform.for_tenant("acme", name="Acme Corp").for_user("u_42", email="pat@example.com", name="Pat Lee")
run = mem.session("Draft the renewal email", harness="renewal-writer")
mem.ingest([{"source": "crm:note:9", "body": "...", "occurred_at": micros(datetime.now(timezone.utc)), "acl": ["user:u_42"]}])

platform.tenants.ensure("acme", name="Acme Corp")  # {tenant, org, created}
platform.tenants.list(limit=100)                   # the platform's tenants and their usage
platform.tenants.forget_user("acme", "u_42")       # a user leaves
platform.tenants.delete("acme")                    # purge the tenant; {purging: true} until it is done
```

Or pass `tenant=`, `tenant_name=`, `user=`, `user_email=` and `user_name=` to the constructor. Tenant and user ids
are the platform's own: 1 to 128 printable ASCII characters, case-sensitive. Email and name are stored on the user
and kept up to date, never their identity. Names and emails can be any text: the client sends them percent-encoded
(UTF-8). A tenant's name only labels it when it is created. Without a user, a request is the tenant's own: it can
ingest and read what the whole tenant shares, but sessions, advice, `remember` and `forget` need a user. ACLs on
what the platform writes name its users as `user:<user id>` and the whole tenant as `group:all`.

Name each of your agents' sessions with its own `harness`, never one ending in `:sdk`: advice isn't learned from
those.

## Methods

| Method | Route | Returns |
|---|---|---|
| `AmpUpMemory(server, token, *, org, team, tenant, tenant_name, user, user_email, user_name, timeout)` | | `token` may be `None` for a local server |
| `AmpUpMemory.from_profile(context=None)` | | a client for a CLI context |
| `for_tenant(tenant, *, name=None)` | | a copy for another tenant, as the tenant itself |
| `for_user(user, *, email=None, name=None)` | | a copy for another user of the same tenant |
| `me()` | `GET /me` | who you are: user, groups, scope, memberships |
| `ingest(episodes)` | `POST /ingest` | every episode id; sent in batches of 500; needs an ingest or admin key |
| `remember(source, body, *, acl, document)` | `POST /remember` | `{episode_id, scope, readers}`, or `{queued, message}` |
| `forget(episode_id)` | `POST /forget` | the forgotten id |
| `search(query, k=10)` | `POST /search` | a list of fact hits |
| `query(q)` | `POST /query` | a query-language result (`find`: entities, facts, count, changes, passages, schema, metrics) |
| `episode(episode_id)` | `POST /episode` | the episode's text, or `None` |
| `session(task, *, harness)` | `POST /sessions` | a `Session`: `id`, `task`, `opening` (the advice served at its start) |
| `Session.advice(question=None, *, moment, conversation)` | `POST /advice` | advice for the task or the step at hand, recorded in the session |
| `Session.end(outcome, reward, *, transcript, reason)` | `POST /sessions/end` | `{ended, served, session_id}` |
| `advice(question)` | `POST /advice` | advice outside any session |
| `reward(run_id, reward, reason=None)` | `POST /runs/reward` | the id; rewards an ended session (written within a minute of its end) or a run |
| `tenants.ensure(tenant, name=None)` | `PUT /tenants/{tenant}` | `{tenant, org, created}` |
| `tenants.get(tenant)` | `GET /tenants/{tenant}` | `{tenant, org, created_at, limits, usage}`; 404 if unknown |
| `tenants.list(after=None, limit=None)` | `GET /tenants` | a page of the platform's tenants with usage (up to 500) |
| `tenants.delete(tenant)` | `DELETE /tenants/{tenant}` | purges the tenant; `{purging: true}` while it runs |
| `tenants.forget_user(tenant, user)` | `POST /tenants/{tenant}/users/{user}/forget` | forgets the user and what they wrote |
| `micros(datetime)` | | `occurred_at` in microseconds since the epoch |

Every failure raises `AmpUpMemoryError` with `status`, `message` and, on a 429, `retry_after`. An error the
server reports inside a row (a bad query, say) raises it too, with status 200.

The TypeScript client with the same methods is [`ampup-memory`](https://github.com/A79-ai/ampup-memory/tree/main/sdk/typescript).

## License

Apache-2.0.
