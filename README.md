# AmpUp Memory

The memory for agents that gets better with use: it learns advice from the runs that went well and serves it
before the next one.

A conversation is a session. Each task in it asks for advice (`get_advice`) and gets one skill for that kind of
task: a note learned from earlier rewarded runs, or, before any exist, a procedure compiled from your documents.
When the session ends, its transcript and outcome are judged or rewarded, and a rewarded session teaches the next
one. Underneath is a cited memory: notes, documents and conversations become searchable passages and facts, each
fact quoting its source and visible only to people who may read every source it cites. Everything is stored as
Apache Iceberg tables in a catalog and bucket you run.

## Install

Build from source; there are no public release binaries yet. You need `rustup` (the toolchain is pinned in
`rust-toolchain.toml`) and `protoc` on your `PATH`. macOS on Apple Silicon or Linux on arm64/x86_64.

```bash
cargo install --locked --path .                                        # ampup-memory: server and CLI
cargo install --locked --path crates/memstore-client --root ~/.local   # ampup-memory-client: hooks for coding agents
```

For Claude Code, add this checkout as a plugin marketplace: `/plugin marketplace add /path/to/ampup-memory`, then
`/plugin install ampup-memory@ampup-memory` ([coding agents](docs/guide/coding-agents.md)). The command was `memstore`
before the rename, and `memstore` still works as a link to it.

## Run it locally

```bash
OPENROUTER_API_KEY=... AMPUP_MEMORY_EXTRACT_GOAL_BLIND=1 ampup-memory serve --local   # ~/.ampup-memory, http://127.0.0.1:8080
claude mcp add --transport http ampup-memory http://localhost:8080/mcp
```

`--local` needs no Docker, catalog service or sign-in, and binds to 127.0.0.1. Without a model key it stores and
searches but learns nothing; any OpenAI-compatible, Anthropic, Bedrock or Vertex AI endpoint, or a local Ollama,
works instead of OpenRouter ([configuration](docs/guide/configuration.md#models)).

### Offline, on your laptop

No key and no network: local embeddings (the default) and a local model through [Ollama](https://ollama.com) for
everything the memory learns.

```bash
ollama pull qwen2.5:3b                 # the default local model
ampup-memory embed warmup              # fetch the embedding model (~130 MB) while you're online
AMPUP_MEMORY_LLM=ollama AMPUP_MEMORY_CLIENT_UPDATE=off ampup-memory serve --local
```

`AMPUP_MEMORY_LLM_MODEL` picks another model, `AMPUP_MEMORY_LLM_URL` another server (any OpenAI-compatible one:
LM Studio, llama.cpp, vLLM). Started in a terminal with nothing configured, `serve --local` asks once whether to
use a key, the local Ollama, or no model, and keeps the answer in `~/.ampup-memory/llm.json`. Without the embedding
model (`HF_HUB_OFFLINE=1`, nothing fetched yet), search ranks by keyword alone. More in the
[quickstart](docs/guide/quickstart.md#offline-on-your-laptop).

## First memory, search and advice

```bash
curl -s localhost:8080/remember -d '{"source": "note:deploy", "body": "Deploy billing with ./deploy.sh billing. Never from a laptop on Fridays."}'
curl -s localhost:8080/query -d '{"data": [[0, {"find": "passages", "text": "deploy billing"}]]}'

curl -s localhost:8080/sessions -d '{"context": "deploy the billing service"}'               # -> session_id
curl -s localhost:8080/advice -d '{"session_id": "<id>", "question": "deploy the billing service"}'
curl -s localhost:8080/sessions/end -d '{"session_id": "<id>", "transcript": "…", "outcome": "deployed", "reward": 1}'
```

A new memory has no advice to give until it has rewarded runs to learn from or documents to compile; the
[quickstart](docs/guide/quickstart.md) walks through both.

HTTP routes: `/remember`, `/search`, `/query`, `/advice`, `/sessions`, `/sessions/end`, `/runs`, `/runs/reward`,
and `/mcp` ([all routes](docs/guide/http-api.md)).

From code, the Python and TypeScript clients wrap the same routes:

```bash
pip install ampup-memory        # Python 3.10+, sdk/python
npm install ampup-memory       # Node 20+, sdk/typescript
```

```python
from ampup_memory import AmpUpMemory

mem = AmpUpMemory("http://127.0.0.1:8080", None)  # a local server; from_profile() uses `ampup-memory-client join`'s sign-in
mem.remember("note:deploy", "Deploy billing with ./deploy.sh billing.")
print(mem.query({"find": "passages", "text": "deploy billing"})["results"])

run = mem.session("deploy the billing service")  # the learning loop: advice per task, rewarded at the end
print(run.opening["advice"])
run.end("deployed", reward=1)
```

## Docs

- [Quickstart](docs/guide/quickstart.md) and [concepts](docs/guide/concepts.md): episodes, facts, sessions,
  advice, rewards.
- [Advice](docs/guide/advice.md): how notes are learned, judged and gated.
- [MCP](docs/guide/mcp.md), [HTTP API](docs/guide/http-api.md), [CLI](docs/guide/cli.md).
- [Schemas and queries](docs/guide/schemas-and-queries.md): typed memory, extraction, search ranking.
- [Self-hosting](docs/guide/self-hosting.md): Iceberg REST (Polaris), BigLake, Snowflake, Databricks; GCS, S3,
  Azure.
- [Configuration](docs/guide/configuration.md) and [security model](docs/guide/security-model.md).
- [Benchmarks](benchmarks/README.md): how each published benchmark was run and scored, with its results.

## Develop

```bash
cargo test    # runs on a local in-memory catalog, servers inside the test process
```

## License

Apache-2.0. See [LICENSE](LICENSE).
