# AmpUp Memory guide

| Page | Read it for |
|---|---|
| [quickstart.md](quickstart.md) | Build, run on a laptop, a first note, search, session and advice. |
| [concepts.md](concepts.md) | Scopes, episodes, facts, commits and branches, sessions, advice, what runs in the background. |
| [advice.md](advice.md) | The learning loop in full: sessions, runs, rewards, judging, notes, the evidence gate, compiled procedures. |
| [coding-agents.md](coding-agents.md) | The Claude Code plugin and hooks; Codex, Gemini CLI, Cursor, Copilot. |
| [mcp.md](mcp.md) | The MCP tools, what each answers, and what the server keeps from users' words. |
| [http-api.md](http-api.md) | Every HTTP route, with bodies and answers. |
| [schemas-and-queries.md](schemas-and-queries.md) | Schemas, extraction, typed queries, passages, ranking, summaries, dreaming, indexes, embeddings. |
| [cli.md](cli.md) | The `ampup-memory` and `ampup-memory-client` commands. |
| [self-hosting.md](self-hosting.md) | Catalogs (local, Iceberg REST, BigLake, Snowflake, Databricks), storage, running several servers, maintenance, backup, metrics. |
| [configuration.md](configuration.md) | Environment variables. |
| [security-model.md](security-model.md) | Identity, ACLs, orgs and teams, API keys, audit, deletion, retention. |

The command is `ampup-memory` throughout (`memstore` before the rename, which still works as a link to it), and the
settings are `AMPUP_MEMORY_*` (each still read under its old `MEMSTORE_*` name when the new one isn't set).
