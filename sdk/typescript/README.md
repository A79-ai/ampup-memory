# @ampup/memory

A thin TypeScript client for an [AmpUp Memory](https://github.com/A79-ai/ampup-memory) server: write what your
team knows, search it, and read it back. One class, no codegen, and no runtime dependencies (global `fetch`,
Node 20+).

```sh
npm install @ampup/memory
```

## Quickstart

```ts
import { AmpUpMemory, micros } from "@ampup/memory";

const mem = await AmpUpMemory.fromProfile(); // the CLI's current context; or AmpUpMemory.fromProfile("work")
// const mem = new AmpUpMemory("https://memory.example.com", "msk_...", { org: "acme" }); // an explicit API key
// const mem = new AmpUpMemory("http://127.0.0.1:8080", null); // a local `ampup-memory serve --local`

const ep = await mem.remember("note:acme", "The Acme renewal is owned by Priya.");
console.log(await mem.episode(ep.episode_id));
console.log(await mem.search("who owns the Acme renewal", 5));
console.log((await mem.query({ find: "passages", text: "Acme renewal" })).results);
await mem.ingest([{ source: "email:1", body: "...", occurred_at: micros(new Date()), acl: ["group:sales"] }]);
```

`fromProfile()` uses the context and sign-in that `ampup-memory-client join <server> --org <org>` saved, in
`AMPUP_MEMORY_CONFIG_DIR` (default `~/.ampup-memory`). It picks the context from its argument, then
`AMPUP_MEMORY_CONTEXT`, then the current one, and renews an expiring sign-in with the saved refresh token, writing
it back where the CLI reads it.

## Methods

| Method | Route | Returns |
|---|---|---|
| `new AmpUpMemory(server, token, {org, team, timeoutMs})` | | `token` may be `null` for a local server |
| `AmpUpMemory.fromProfile(context?)` | | a client for a CLI context |
| `me()` | `GET /me` | who you are: user, groups, scope, memberships |
| `ingest(episodes)` | `POST /ingest` | every episode id; sent in batches of 500; needs an ingest or admin key |
| `remember(source, body, {acl, document})` | `POST /remember` | `{episode_id, scope, readers}`, or `{queued, message}` |
| `forget(episodeId)` | `POST /forget` | the forgotten id |
| `search(query, k = 10)` | `POST /search` | a list of fact hits |
| `query(q)` | `POST /query` | a query-language result (`find`: entities, facts, count, changes, passages, schema, metrics) |
| `episode(episodeId)` | `POST /episode` | the episode's text, or `null` |
| `micros(date)` | | `occurred_at` in microseconds since the epoch |

Every failure throws `AmpUpMemoryError` with `status`, `message` and, on a 429, `retryAfter`. An error the server
reports inside a row (a bad query, say) throws it too, with status 200.

The Python client with the same methods is [`ampup-memory`](https://github.com/A79-ai/ampup-memory/tree/main/sdk/python) on PyPI.

## License

Apache-2.0.
