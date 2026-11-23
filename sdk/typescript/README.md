# ampup-memory

A thin TypeScript client for an [AmpUp Memory](https://github.com/A79-ai/ampup-memory) server: write what your
team knows, search it, and get advice that improves as sessions are rewarded. One class, no codegen, and no runtime dependencies (global `fetch`,
Node 20+).

```sh
npm install ampup-memory
```

## Quickstart

```ts
import { AmpUpMemory, micros } from "ampup-memory";

const mem = await AmpUpMemory.fromProfile(); // the CLI's current context; or AmpUpMemory.fromProfile("work")
// const mem = new AmpUpMemory("https://memory.example.com", "msk_...", { org: "acme" }); // an API key: yours (apikey create --personal) or a service's
// const mem = new AmpUpMemory("http://127.0.0.1:8080", null); // a local `ampup-memory serve --local`

const ep = await mem.remember("note:acme", "The Acme renewal is owned by Priya.");
console.log(await mem.episode(ep.episode_id));
console.log(await mem.search("who owns the Acme renewal", 5));
console.log((await mem.query({ find: "passages", text: "Acme renewal" })).results);
await mem.ingest([{ source: "email:1", body: "...", occurred_at: micros(new Date()), acl: ["group:sales"] }]);
```

## The learning loop

A session per task: advice as it goes, and how it went at the end. Advice is learned from sessions that ended with
a reward, and served to the next session of that kind of task.

```ts
const run = await mem.session({ task: "Handle a refund request for order 8841", harness: "support-bot" });
console.log(run.opening.advice); // what the memory has learned for this kind of task, or null
console.log((await run.advice({ question: "Issue the $620 refund", moment: "next" })).advice); // next, fork, stuck, done
await run.end({ outcome: "refund issued after sign-off", reward: 1 }); // -1 (failed) to 1 (succeeded)

await mem.reward(run.id, -1, "the customer disputed it"); // or reward it later, once you know
```

Advice needs a server with a routing model: a hosted one, or `serve --local` with a model key (outside a session,
`advice()` throws a 503 without one). A `harness` ending in `:sdk` marks a session a program's: advice isn't
learned from it.

`fromProfile()` uses the context and sign-in that `ampup-memory-client join <server> --org <org>` saved, in
`AMPUP_MEMORY_CONFIG_DIR` (default `~/.ampup-memory`). It picks the context from its argument, then
`AMPUP_MEMORY_CONTEXT`, then the current one, and renews an expiring sign-in with the saved refresh token, writing
it back where the CLI reads it.

## Serving many tenants (platforms)

A platform (an app that serves many customers of its own) uses one platform key for all of them. Each request names
the tenant, by the platform's own id for that customer, and, optionally, the user acting in it. Memory creates a
tenant the first time a request names it and enrolls a user the same way; each tenant's memory, and what it learns,
stays inside it.

```ts
const platform = new AmpUpMemory("https://memory.example.com", "msk_..."); // the platform key, on your server only

// Per request: a copy for that tenant and user.
const mem = platform.forTenant("acme", { name: "Acme Corp" }).forUser("u_42", { email: "pat@example.com", name: "Pat Lee" });
const run = await mem.session({ task: "Draft the renewal email", harness: "renewal-writer" });
await mem.ingest([{ source: "crm:note:9", body: "...", occurred_at: micros(new Date()), acl: ["user:u_42"] }]);

await platform.tenants.ensure("acme", { name: "Acme Corp" }); // {tenant, org, created}
await platform.tenants.list({ limit: 100 }); // the platform's tenants and their usage
await platform.tenants.forgetUser("acme", "u_42"); // a user leaves
await platform.tenants.delete("acme"); // purge the tenant; {purging: true} until it is done
```

Or pass `tenant`, `tenantName`, `user`, `userEmail` and `userName` to the constructor. Tenant and user ids are the
platform's own: 1 to 128 printable ASCII characters, case-sensitive. Email and name are stored on the user and kept
up to date, never their identity. Names and emails can be any text: the client sends them percent-encoded (UTF-8). A
tenant's name only labels it when it is created. Without a user, a request is the tenant's own: it can ingest and
read what the whole tenant shares, but sessions, advice, `remember` and `forget` need a user. ACLs on what the
platform writes name its users as `user:<user id>` and the whole tenant as `group:all`.

Name each of your agents' sessions with its own `harness`, never one ending in `:sdk`: advice isn't learned from
those.

## Methods

| Method | Route | Returns |
|---|---|---|
| `new AmpUpMemory(server, token, {org, team, tenant, tenantName, user, userEmail, userName, timeoutMs})` | | `token` may be `null` for a local server |
| `AmpUpMemory.fromProfile(context?)` | | a client for a CLI context |
| `forTenant(tenant, {name})` | | a copy for another tenant, as the tenant itself |
| `forUser(user, {email, name})` | | a copy for another user of the same tenant |
| `me()` | `GET /me` | who you are: user, groups, scope, memberships |
| `ingest(episodes)` | `POST /ingest` | every episode id; sent in batches of 500; needs an ingest or admin key |
| `remember(source, body, {acl, document})` | `POST /remember` | `{episode_id, scope, readers}`, or `{queued, message}` |
| `forget(episodeId)` | `POST /forget` | the forgotten id |
| `search(query, k = 10)` | `POST /search` | a list of fact hits |
| `query(q)` | `POST /query` | a query-language result (`find`: entities, facts, count, changes, passages, schema, metrics) |
| `episode(episodeId)` | `POST /episode` | the episode's text, or `null` |
| `session({task, harness})` | `POST /sessions` | a `Session`: `id`, `task`, `opening` (the advice served at its start) |
| `session.advice({question, moment, conversation})` | `POST /advice` | advice for the task or the step at hand, recorded in the session |
| `session.end({outcome, reward, transcript, reason})` | `POST /sessions/end` | `{ended, served, session_id}` |
| `advice(question)` | `POST /advice` | advice outside any session |
| `reward(runId, reward, reason?)` | `POST /runs/reward` | the id; rewards an ended session (written within a minute of its end) or a run |
| `tenants.ensure(tenant, {name})` | `PUT /tenants/{tenant}` | `{tenant, org, created}` |
| `tenants.get(tenant)` | `GET /tenants/{tenant}` | `{tenant, org, created_at, limits, usage}`; 404 if unknown |
| `tenants.list({after, limit})` | `GET /tenants` | a page of the platform's tenants with usage (up to 500) |
| `tenants.delete(tenant)` | `DELETE /tenants/{tenant}` | purges the tenant; `{purging: true}` while it runs |
| `tenants.forgetUser(tenant, user)` | `POST /tenants/{tenant}/users/{user}/forget` | forgets the user and what they wrote |
| `micros(date)` | | `occurred_at` in microseconds since the epoch |

Every failure throws `AmpUpMemoryError` with `status`, `message` and, on a 429, `retryAfter`. An error the server
reports inside a row (a bad query, say) throws it too, with status 200.

The Python client with the same methods is [`ampup-memory`](https://github.com/A79-ai/ampup-memory/tree/main/sdk/python) on PyPI.

## License

Apache-2.0.
