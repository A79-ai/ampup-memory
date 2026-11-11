# Quickstart

From a checkout of this repository to a running memory, a first note, a first search and a first piece of advice.
Everything here runs on one machine with no cloud account.

## 1. Build

You need:

- Rust, through `rustup`. The toolchain is pinned in `rust-toolchain.toml` (1.99.1) and installs itself on the
  first `cargo` command.
- `protoc` (the Protocol Buffers compiler) on your `PATH`, as the CI builds install it
  (`brew install protobuf`, `apt install protobuf-compiler`).
- macOS on Apple Silicon or Linux on arm64 or x86_64. The ONNX Runtime behind local embeddings is downloaded
  prebuilt during the build; there is none for Intel macOS.

```bash
cargo install --locked --path .                                    # ampup-memory: server and CLI
cargo install --locked --path crates/memstore-client --root ~/.local   # ampup-memory-client: hooks and MCP bridge
```

There are no public release binaries yet.

## 2. Run it

```bash
ampup-memory serve --local
# ampup-memory: serving memory_org from ~/.ampup-memory on http://127.0.0.1:8080/mcp (as group:local)
```

Everything lives under `~/.ampup-memory` (pass a directory to `--local` for another). Nobody signs in; the server binds
to 127.0.0.1 only. `curl localhost:8080/ready` answers `{"ready":true,…}` once it has warmed up.

With no model key, the server stores and searches but extracts, compiles and learns nothing. To turn the model work
on, start it with a key:

```bash
OPENROUTER_API_KEY=... AMPUP_MEMORY_EXTRACT_GOAL_BLIND=1 ampup-memory serve --local
```

`AMPUP_MEMORY_EXTRACT_GOAL_BLIND=1` reads every episode into facts. Other providers (Anthropic, OpenAI-compatible,
Bedrock, Vertex AI, a local Ollama) are in [configuration.md](configuration.md#models).

## 3. A first memory

```bash
curl -s localhost:8080/remember -d '{"source": "note:deploy",
  "body": "Deploy billing with ./deploy.sh billing. Never deploy from a laptop on Fridays."}'
# {"episode_id":"9b26c2…","readers":"everyone who reads org","scope":"org"}
```

## 4. A first search

Passages (the text itself) are searchable at once, with or without a model:

```bash
curl -s localhost:8080/query -d '{"data": [[0, {"find": "passages", "text": "deploy billing", "limit": 3}]]}'
# {"data":[[0,{"find":"passages","results":[{"id":"9b26c2aadfb65ce3#0","snippet":"Deploy billing with ./deploy.sh billing. …", …}], …}]]}
```

Facts (`POST /search`) appear once a model has extracted them. Over MCP, `search_memory` shows both: the facts, and
the source text no fact cites yet.

## 5. Connect an agent

```bash
claude mcp add --transport http ampup-memory http://localhost:8080/mcp
```

The agent is offered `start_session`, `end_session`, `search_memory`, `get_episode`, `remember_memory` and
`forget_memory`, plus `get_advice` once the server has a model. For hooks that drive the loop without the model
choosing to, install the Claude Code plugin: [coding-agents.md](coding-agents.md).

## 6. A first session and first advice

Advice needs a model key (step 2). Without one, `POST /advice` answers 503.

```bash
curl -s localhost:8080/sessions -d '{"context": "deploy the billing service", "harness": "curl"}'
# {"advice":null,"session_id":"6512edd0-…","situation":null}

curl -s localhost:8080/advice -d '{"session_id": "6512edd0-…", "question": "deploy the billing service"}'

curl -s localhost:8080/sessions/end -d '{"session_id": "6512edd0-…",
  "transcript": "Asked to deploy billing. Ran ./deploy.sh billing from CI.", "outcome": "deployed", "reward": 1}'
# {"ended":true,"served":[],"session_id":"6512edd0-…"}
```

A new memory has nothing to serve yet: `situation` is null. Advice appears from one of two sources.

**Procedures compiled from your documents.** Name the sources to compile, and `get_advice` serves the matching
procedure before any runs exist. Notes sent to `/remember` are stored under the source
`mcp:remember_memory:<caller>:<source>`, so this compiles every remembered note:

```bash
OPENROUTER_API_KEY=... AMPUP_MEMORY_COMPILE_SOURCES=mcp:remember_memory: ampup-memory serve --local
curl -s localhost:8080/remember --data-binary @- <<'EOF'
{"source": "note:runbook", "body": "To deploy billing: run ./deploy.sh billing from CI, check /healthz, then post in #deploys."}
EOF
```

The server compiles new sources every `AMPUP_MEMORY_COMPILE_SECS` (60). Compiling runs on the learning model (Claude
Opus 5.5 by default), so name sources narrowly.

**Notes learned from rewarded runs.** Record runs with their outcomes, then learn:

```bash
cat > runs.jsonl <<'EOF'
{"task": "deploy billing", "trace": "ran ./deploy.sh billing in CI", "outcome": "deployed", "reward": 1, "reason": "deployed"}
{"task": "deploy billing on Friday", "trace": "deployed from a laptop", "outcome": "rolled back", "reward": -1, "reason": "never deploy from a laptop"}
EOF
# with the server stopped
ampup-memory runs load org runs.jsonl --local ~/.ampup-memory --as amy@example.com
ampup-memory runs load org runs.jsonl --local ~/.ampup-memory --as ben@example.com
OPENROUTER_API_KEY=... ampup-memory advice org --local ~/.ampup-memory
OPENROUTER_API_KEY=... AMPUP_MEMORY_ADVICE_EVIDENCE=0 ampup-memory serve --local
```

Rules this example works around, on purpose:

- A team note is learned only from runs of at least 2 people (`AMPUP_MEMORY_ADVICE_LEARN_PEOPLE`, default 3, never
  below 2). `serve --local` has one anonymous caller, so the runs are loaded as two people.
- A situation needs at least 2 runs that went wrong (`--min-wrong`) or enough that went right.
- A new note is a candidate, served only in 20% of sessions, until it has helped 3 people.
  `AMPUP_MEMORY_ADVICE_EVIDENCE=0` serves it everywhere on this laptop.

On a running `serve --local`, the same happens on its own: rewarded sessions and runs come in, and every 10 minutes
the server learns once 10 new rewarded ones are waiting. See [advice.md](advice.md).

Not verified end to end for this guide: the compile and learn steps above (they call a paid model). The build, the
server, `/remember`, the passage search, `/sessions`, `/sessions/end`, `/runs`, `/runs/reward`, `runs load` and the
503 from `/advice` without a model were run against this tree.

## Next

- [concepts.md](concepts.md): scopes, episodes, facts, sessions, advice.
- [self-hosting.md](self-hosting.md): a real catalog over S3, GCS or Azure.
- [security-model.md](security-model.md): sign-in, keys, ACLs, deletion.
