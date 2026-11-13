# Quickstart

From nothing to a running memory, a first note, a first search and a first piece of advice. Everything here runs on
one machine with no cloud account.

## 1. Install

```bash
uv tool install ampup-memory-cli      # or: pipx install ampup-memory-cli, or pip install ampup-memory-cli
```

That installs `ampup-memory` (server and CLI) and `ampup-memory-client` on your `PATH` from
[`ampup-memory-cli`](https://pypi.org/project/ampup-memory-cli/) on PyPI, a wheel per platform holding the two
binaries (no Python code runs). `uv tool upgrade ampup-memory-cli` updates them. To try it without installing
anything, `uvx --from ampup-memory-cli ampup-memory serve --local` runs step 2.

### Or with the installer

```bash
curl -fsSL https://raw.githubusercontent.com/A79-ai/ampup-memory/main/install.sh | sh
```

That installs the same two into `~/.local/bin` from the newest
[release](https://github.com/A79-ai/ampup-memory/releases) (PyPI gets one a week), with only `curl` (or `wget`) and
`tar`. Set `AMPUP_MEMORY_INSTALL_DIR` for another directory, or `AMPUP_MEMORY_VERSION` for a given release.

### Or build from source

You need:

- Rust, through `rustup`. The toolchain is pinned in `rust-toolchain.toml` (1.99.1) and installs itself on the
  first `cargo` command.
- `protoc` (the Protocol Buffers compiler) on your `PATH`, as the CI builds install it
  (`brew install protobuf`, `apt install protobuf-compiler`).
- macOS (Apple Silicon or Intel) or Linux on arm64 or x86_64. The ONNX Runtime behind local embeddings is
  downloaded prebuilt during the build; there is none for Intel macOS, where CONTRIBUTING.md builds it first.

```bash
cargo install --locked --path .                                    # ampup-memory: server and CLI
cargo install --locked --path crates/memstore-client --root ~/.local   # ampup-memory-client: hooks and MCP bridge
```

## 2. Run it

```bash
ampup-memory serve --local
# ampup-memory: serving memory_org from ~/.ampup-memory on http://127.0.0.1:8080/mcp (as group:local)
```

Everything lives under `~/.ampup-memory` (pass a directory to `--local` for another). Nobody signs in; the server binds
to 127.0.0.1 only. `curl localhost:8080/ready` answers `{"ready":true,…}` once it has warmed up.

To keep the server always on without a terminal open, install it as a background service instead:

```bash
ampup-memory service install    # starts now, at login, and again if it stops
ampup-memory service status     # running, pid, port, data, log, and whether it answers
```

`ampup-memory service uninstall` stops it; the memory stays. See [cli.md](cli.md#the-background-service).

With a coding agent's hooks (the Claude Code plugin, or `ampup-memory hooks install`) you needn't do either: where no
team server is named, a session's start runs `ampup-memory serve --local --background` when nothing answers on
port 8080 ([coding-agents.md](coding-agents.md#claude-code); `AMPUP_MEMORY_AUTOSTART=0` turns it off). Where no
model is saved yet, that first start saves the one the agent's environment names in `~/.ampup-memory/llm.json`, as
`service install` does.

With no model key, the server stores and searches but extracts, compiles and learns nothing. To turn the model work
on, start it with a key:

```bash
OPENROUTER_API_KEY=... AMPUP_MEMORY_EXTRACT_GOAL_BLIND=1 ampup-memory serve --local
```

`AMPUP_MEMORY_EXTRACT_GOAL_BLIND=1` reads every episode into facts. Other providers (Anthropic, OpenAI-compatible,
Bedrock, Vertex AI, a local Ollama) are in [configuration.md](configuration.md#models).

### Offline, on your laptop

Everything can run with no network and no key: embeddings with a small local model (bge-small, about 130 MB, the
default when no embeddings key is set), and every model call (extracting, compiling, learning advice, judging,
routing) through [Ollama](https://ollama.com) or any OpenAI-compatible server on your machine.

```bash
ollama pull qwen2.5:3b              # the default local model, a 1.9 GB download
ampup-memory embed warmup           # fetches the embedding model now, while you have a network
AMPUP_MEMORY_LLM=ollama AMPUP_MEMORY_CLIENT_UPDATE=off ampup-memory serve --local
# ampup-memory: every model call goes to Ollama at http://localhost:11434 (qwen2.5:3b, context 32768 tokens)
# ampup-memory: embeddings: local Xenova/bge-small-en-v1.5 (384 dims), loaded from ~/.cache/memstore/models
```

- `AMPUP_MEMORY_LLM=ollama` sends every role to Ollama, on one model: `AMPUP_MEMORY_LLM_MODEL` names another
  (`qwen2.5:7b` or larger learns better advice, where the machine has the memory), `AMPUP_MEMORY_LLM_URL` another
  address. A role's own variables (`AMPUP_MEMORY_EXTRACT`, `AMPUP_MEMORY_ADVICE_MODEL`, ...) still win.
- Ollama is called through its own API with a 32,768-token context (`AMPUP_MEMORY_LLM_CONTEXT`), so the longest
  prompts (60,000 characters) aren't cut off: its OpenAI-compatible endpoint ignores a request's context size, and
  its default is as low as 4,096 tokens on a machine with little memory.
- Another local server (LM Studio, llama.cpp's server, vLLM): `AMPUP_MEMORY_LLM_URL=http://localhost:1234/v1` and
  `AMPUP_MEMORY_LLM_MODEL=<its model>`. Set the server's own context to 32k tokens or more.
- With a local model, no decisions model is asked (they are OpenAI's and OpenRouter's alone, and
  `AMPUP_MEMORY_DECIDE` still names one): kept notes are matched to a task by shared words, and the learning model
  sorts and merges advice alone.
- `HF_HUB_OFFLINE=1` never fetches the embedding model; `AMPUP_MEMORY_MODEL_DIR` (or `HF_HOME`) says where it is
  kept, and `HF_ENDPOINT` names a mirror to fetch it from. Until the model loads, searches rank by keyword alone
  rather than failing.
- `AMPUP_MEMORY_CLIENT_UPDATE=off` keeps the hooks' client from checking for a new release.

Run `ampup-memory serve --local` in a terminal with nothing configured and it asks once: paste an OpenRouter,
OpenAI or Anthropic key, use the local Ollama (listing its models), or continue without learning. The answer is
kept in `~/.ampup-memory/llm.json` (delete it to be asked again); environment variables override it. A key typed
there is not shown and only goes to that provider: embeddings stay local. Without a terminal nothing is asked, and
the server says how to turn learning on.

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

A new memory has nothing to serve yet: `situation` is null. With the Claude Code plugin (or the hooks
`ampup-memory hooks install` writes), a session's start in a git repository sends its docs and recent pull requests
once a day, and the server writes notes for the usual coding tasks from them, so the first session there is
advised already ([advice.md](advice.md#advice-from-a-repository-from-the-first-session)). Otherwise advice appears
from one of two sources.

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
OPENROUTER_API_KEY=... ampup-memory serve --local
```

Rules this example works around, on purpose:

- A team note is learned only from runs of at least 2 people (`AMPUP_MEMORY_ADVICE_LEARN_PEOPLE`, default 3, never
  below 2). `serve --local` has one anonymous caller, so the runs are loaded as two people.
- A situation needs at least 2 runs that went wrong (`--min-wrong`) or enough that went right.

On a running `serve --local`, the same happens on its own: rewarded sessions and runs come in, and every 10 minutes
the server learns once 10 new rewarded ones are waiting. See [advice.md](advice.md).

Not verified end to end for this guide: the compile and learn steps above (they call a paid model). The build, the
server, `/remember`, the passage search, `/sessions`, `/sessions/end`, `/runs`, `/runs/reward`, `runs load` and the
503 from `/advice` without a model were run against this tree.

## Next

- [concepts.md](concepts.md): scopes, episodes, facts, sessions, advice.
- [self-hosting.md](self-hosting.md): a real catalog over S3, GCS or Azure.
- [security-model.md](security-model.md): sign-in, keys, ACLs, deletion.
