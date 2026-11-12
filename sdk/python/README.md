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
# mem = AmpUpMemory("https://memory.example.com", "msk_...", org="acme")  # an explicit API key
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

## Methods

| Method | Route | Returns |
|---|---|---|
| `AmpUpMemory(server, token, *, org, team, timeout)` | | `token` may be `None` for a local server |
| `AmpUpMemory.from_profile(context=None)` | | a client for a CLI context |
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
| `micros(datetime)` | | `occurred_at` in microseconds since the epoch |

Every failure raises `AmpUpMemoryError` with `status`, `message` and, on a 429, `retry_after`. An error the
server reports inside a row (a bad query, say) raises it too, with status 200.

The TypeScript client with the same methods is [`ampup-memory`](https://github.com/A79-ai/ampup-memory/tree/main/sdk/typescript).

## License

Apache-2.0.
