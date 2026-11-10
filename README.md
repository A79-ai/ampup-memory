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
cargo install --locked --path .                                        # memstore: server and CLI
cargo install --locked --path crates/memstore-client --root ~/.local   # memstore-client: hooks for coding agents
```

For Claude Code, add this checkout as a plugin marketplace: `/plugin marketplace add /path/to/ampup-memory`, then
`/plugin install memstore@memstore` ([coding agents](docs/guide/coding-agents.md)). The command is `memstore` for
now; it will become `ampup-memory`.

## Run it locally

```bash
OPENROUTER_API_KEY=... MEMSTORE_EXTRACT_GOAL_BLIND=1 memstore serve --local   # ~/.memstore, http://127.0.0.1:8080
claude mcp add --transport http memstore http://localhost:8080/mcp
```

`--local` needs no Docker, catalog service or sign-in, and binds to 127.0.0.1. Without a model key it stores and
searches but learns nothing; any OpenAI-compatible, Anthropic, Bedrock or Vertex AI endpoint works instead of
OpenRouter ([configuration](docs/guide/configuration.md#models)).

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

## Docs

- [Quickstart](docs/guide/quickstart.md) and [concepts](docs/guide/concepts.md): episodes, facts, sessions,
  advice, rewards.
- [Advice](docs/guide/advice.md): how notes are learned, judged and gated.
- [MCP](docs/guide/mcp.md), [HTTP API](docs/guide/http-api.md), [CLI](docs/guide/cli.md).
- [Schemas and queries](docs/guide/schemas-and-queries.md): typed memory, extraction, search ranking.
- [Self-hosting](docs/guide/self-hosting.md): Iceberg REST (Polaris), BigLake, Snowflake, Databricks; GCS, S3,
  Azure.
- [Configuration](docs/guide/configuration.md) and [security model](docs/guide/security-model.md).

## Develop

```bash
cargo test    # runs on a local in-memory catalog, servers inside the test process
```

## License

Apache-2.0. See [LICENSE](LICENSE).
