# Coding agents

The plugin in [`plugins/memstore`](../../plugins/memstore) closes the learning loop in Claude Code with hooks, so
it never depends on the model choosing to call a tool. The same hooks install into Codex, Gemini CLI, Cursor and
Copilot with one command.

There are no public release binaries yet. Build the client from source.

## Claude Code

1. Build `memstore-client`, the small binary the plugin runs (sign-in, hooks, MCP bridge), into `~/.local/bin`,
   where the plugin looks first:

   ```bash
   cargo install --locked --path crates/memstore-client --root ~/.local
   ```

2. Run a server. On a laptop:

   ```bash
   MEMSTORE_EXTRACT_GOAL_BLIND=1 OPENROUTER_API_KEY=... memstore serve --local
   ```

   With no server named and no sign-in, the hooks use `http://127.0.0.1:8080`. For a team server, sign in once:
   `memstore-client join https://memory.example.com --org acme`.

3. Add this repository as a plugin marketplace and install the plugin (in Claude Code):

   ```text
   /plugin marketplace add /path/to/ampup-memory
   /plugin install memstore@memstore
   ```

4. Check it: `/memstore:status` in Claude Code, or `memstore-client doctor` in a terminal.

The plugin's launcher (`plugins/memstore/bin/memstore-client`) downloads a client from a releases repository when
none is installed, and checks once a day for a newer one. With a client built from source, set
`MEMSTORE_CLIENT_UPDATE=off` to keep it.

### What the hooks do

- **A session per Claude Code session.** SessionStart opens a memstore session and puts its id in context.
  Compaction and resume continue it. Stop sends the conversation so far, at most every five minutes, without
  holding up the turn.
- **Advice at the first prompt.** The prompt hook asks `get_advice` for the task, fitted to its share of the
  context (`max_chars`), and adds the answer ahead of the recalled facts with an instruction to follow it unless
  the evidence contradicts it. The call runs in the background; the advice reaches Claude with the next hook.
- **Advice at each step.** After a turn with tool calls, or a tool call that usually ends a step (a commit, a push,
  a build or test run), the hook asks for the `next` moment. The note already served is re-read (no model call) and
  passed on only if a decisions model finds it useful now (`MEMSTORE_ADVICE_USEFUL_MIN`, 0.4). At most one ask
  per 30 seconds and 12 pieces of advice per session.
- **Recall.** Facts the memory held about the repo are added at session start; the first prompt's best-matching
  facts come with the advice.
- **Ended with its trace.** SessionEnd ends the session with a trace (each prompt, each answer shortened, each tool
  call with shortened arguments and the first 200 characters of its result, under 20,000 characters) and Claude's
  last answer as the outcome. Only its person can read the trace.
- **Notes.** The session is also remembered as a condensed note every five prompts, before compaction and at the
  end.
- **Starter memory.** At session start the repo's `CLAUDE.md`, `.claude/CLAUDE.md`, `AGENTS.md`, `GEMINI.md`,
  `.claude/rules` and `.cursor/rules`, and your Claude Code memory files for the repo, are imported into your own
  memory (`?scope=mine`), which no teammate reads. At most 40 files of up to 64 KB; a file is sent again only when
  it changed. `MEMSTORE_STARTER=off` imports nothing.
- **Never waits on the memory.** A slow or down server costs a session its context, never its time.

Everything the hooks send has secret-looking words redacted (known key prefixes, JWTs, long random strings, the
values of `password=`, `token:` and similar).

### Commands

| Command | What it does |
|---|---|
| `/memstore:login [<server>] [--org <org>]` | Signs in through the browser. |
| `/memstore:outcome good\|bad [why]` | Rewards the session +1 or -1; your words are the reason. |
| `/memstore:remember <text>` | Remembers a note. |
| `/memstore:search <words>` | Searches the memory. |
| `/memstore:brief <name> [refresh]` | Where a subject stands, where the schema summarises it. |
| `/memstore:forget <episode id or words>` | Forgets something you remembered. |
| `/memstore:status` | Whether it all works: server, sign-in, org, what runs there, how the last hook went. |

In a terminal, `memstore-client hook outcome good|bad [why]` rates the last session the hooks opened.

### Variables

| Variable | Default | What it does |
|---|---|---|
| `MEMSTORE_URL` | the CLI context's server, else `http://127.0.0.1:8080` | Where the memory is. |
| `MEMSTORE_ORG` | the CLI context's org | The org, with `MEMSTORE_URL`. |
| `MEMSTORE_TOKEN` | the CLI's sign-in | A bearer to send instead (an API key, for CI). |
| `MEMSTORE_SCOPE` | the team's | `mine` keeps sessions, notes and recall in your own memory. |
| `MEMSTORE_CAPTURE` | on | `off`: no notes, no session trace. Advice and recall still work. |
| `MEMSTORE_RECALL` | first prompt | `every` recalls facts on every prompt. |
| `MEMSTORE_INSTALL_DIR` | `~/.local/bin` | Where the plugin looks for `memstore-client`. |
| `MEMSTORE_CLIENT_UPDATE` | on | `off` keeps the installed client. |

An organization can set the plugin, `MEMSTORE_URL` and `MEMSTORE_ORG` for everyone in Claude Code's managed
settings (`enabledPlugins`, `env`).

## Codex, Gemini CLI, Cursor and Copilot

```bash
memstore hooks install --harness codex      # or gemini, cursor, copilot; --url <server>; --dry-run first
memstore hooks uninstall --harness codex
```

| Harness | Files | Recall |
|---|---|---|
| codex | `~/.codex/hooks.json`, `[mcp_servers.memstore]` in `~/.codex/config.toml` | session start; advice and facts at the first prompt |
| gemini | `hooks` and `mcpServers` in `~/.gemini/settings.json` | session start; advice and facts at the first prompt |
| cursor | `~/.cursor/hooks.json`, `~/.cursor/mcp.json` | session start only (its prompt hook can't add context) |
| copilot | `~/.copilot/hooks/memstore.json`, `~/.copilot/mcp-config.json` | session start only |

The installer keeps everything else in those files, backs each up before its first change
(`<file>.memstore-backup`), and changes nothing when run twice. Those transcripts are read for text only, so their
traces hold no tool calls.

`memstore setup <server> --org <org> [--harness codex,cursor] [--dry-run]` signs in and sets up every agent it
finds in one go, then runs `memstore doctor`.

## Hosts without hooks

Claude Desktop, ChatGPT and other MCP clients use the server directly: point them at `<server>/mcp`, or run
`memstore mcp` where the host starts a command. The
[`memstore-advice`](../../plugins/memstore/skills/memstore-advice/SKILL.md) skill describes the loop for them; add
it as a skill or put it in the agent's instructions.
