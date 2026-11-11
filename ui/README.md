# AmpUp Memory UI

A browser for what an AmpUp Memory server remembers. It reads AmpUp Memory's `/browse`, `/search` and `/mcp` endpoints from its
own route handlers, so the browser never talks to AmpUp Memory directly. Pick a server and a scope, then:

- **Browse** what is remembered as file trees: `sources/` (episodes by where they came from, a source
  `slack:C09:1712.3` at `sources/slack/C09/1712.3`), `entities/` (facts by what they are about, `deal:42`
  at `entities/deal/42`) and, where the scope has compiled any, `skills/` (its routing index and each skill and
  when it applies; from a server that compiled per goal, a folder per goal). Open an episode to read its text and the facts drawn from it, an
  entity to see its facts over time and the episodes behind each, or a skill to read it with its AIP step
  graph. It reads AmpUp Memory's `/browse` (see the repo README, "Browsing memory as a tree").
- **Branch**: what is read is a branch of the scope, main unless picked. A build may compile skills on a
  branch of its own (`ampup-memory compile --branch`), so the picker counts each branch's skills, and on a branch
  with none the page says which have them. A server whose `/browse` predates `skills/` (one pinned to an
  older build) shows them from its MCP `skills` tool instead; it can't list its branches, so the branch is typed.
- **Schema**: the memory's schema as cards: the source prefixes its `extract` section reads, the versions
  committed on the branch (with the prefixes each added or dropped), every type with its predicates, the guidance,
  and the raw JSON. It reads `/browse`'s `schema` op.
- **Tools**: call the memory's MCP tools (`search_memory`, `query_memory`, `get_entity`, `get_episode`, and
  any others the server offers) with JSON arguments, and see what an agent would get.

Needs Node 22.

```bash
cd ui
npm install
npm run dev -- -p 3100        # http://localhost:3100
```

The UI reaches AmpUp Memory in one of two ways.

## Kubernetes namespaces (the default)

With `AMPUP_MEMORY_URL` unset, the route handlers (`src/app/api/`) run `kubectl` with your own credentials: they list
the namespaces named `memstore-*`, and port-forward to a namespace's `memstore-serve` Deployment (port 8080) when
it is first read (`src/lib/gke.ts`). Nothing is exposed beyond this machine, and you see the namespaces your own
cluster access allows. Each such serve is expected to identify no one and read any scope in its catalog
(`AMPUP_MEMORY_SERVE_ANY_SCOPE=1`), which the scope picker lists.

| Variable | |
|---|---|
| `AMPUP_MEMORY_KUBE_CONTEXT` | the cluster's kubectl context (default: kubectl's current context) |

A namespace shows something only while its `memstore-serve` runs an AmpUp Memory server with `/browse`; one built before
says so instead.

## One AmpUp Memory server, as the signed-in person

With `AMPUP_MEMORY_URL` set (a hosted UI, for example on Vercel with Root Directory `ui/`), the UI reads that one
AmpUp Memory server as the person signed in with Google: each request carries their own Google ID token, so AmpUp Memory decides
what they may read, as for any other caller (`src/lib/auth.ts`). The server must accept Google ID tokens for
the UI's OAuth client.

| Variable | |
|---|---|
| `AMPUP_MEMORY_URL` | the AmpUp Memory server to read |
| `AMPUP_MEMORY_LABEL` | optional: its name in the picker (default its host) |
| `AUTH_GOOGLE_ID`, `AUTH_GOOGLE_SECRET` | the OAuth client (a Web application, redirect URI `<site>/api/auth/callback/google`) |
| `NEXTAUTH_SECRET` | encrypts the session cookie |
| `AUTH_ALLOWED_DOMAINS` | optional: only these email domains may sign in |

A hosted UI can also list a cluster's namespaces through a gateway: this same app run inside the cluster
(`Dockerfile`), where `kubectl` uses the pod's service account. Set `GATEWAY_AUDIENCE` (the UI's OAuth client ID)
and `AUTH_ALLOWED_DOMAINS` on the gateway, and `AMPUP_MEMORY_GATEWAY_URL` (the gateway's URL) on the hosted UI
(`src/lib/gateway.ts`).

## Links

`?ns=<namespace>&scope=<scope>&branch=<branch>`, then `#/f/<path>` a folder, `#/e/entities/<type>/<key>`
an entity, `#/s/skills/<slug>` a compiled skill (`…/index` the routing index), `#/x/<episode id>`
an episode, `#/q/<text>` a search, `#/t/<tool>` the Tools tab, `#/c` the Schema tab.
