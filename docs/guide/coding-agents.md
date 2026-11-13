# Coding agents

The plugin in [`plugins/ampup-memory`](../../plugins/ampup-memory) closes the learning loop in Claude Code with hooks, so
it never depends on the model choosing to call a tool. The same hooks install into Codex, Gemini CLI, Cursor and
Copilot with one command.

## Claude Code

1. Nothing to build: the plugin runs `ampup-memory-client`, a small binary (sign-in, hooks, MCP bridge), and
   downloads it from the [public releases](https://github.com/A79-ai/ampup-memory/releases) when none is installed.
   To install it ahead of time, or with the server:

   ```bash
   curl -fsSL https://raw.githubusercontent.com/A79-ai/ampup-memory/main/install.sh | sh
   ```

   A client built from source (`cargo install --locked --path crates/memstore-client --root ~/.local`) also works;
   the plugin looks in `~/.local/bin` first.

2. Run a server. On a laptop:

   ```bash
   AMPUP_MEMORY_EXTRACT_GOAL_BLIND=1 OPENROUTER_API_KEY=... ampup-memory serve --local
   ```

   With no server named and no sign-in, the hooks use `http://127.0.0.1:8080`. For a team server, sign in once:
   `ampup-memory-client join https://memory.example.com --org acme`.

3. Add this repository as a plugin marketplace and install the plugin (in Claude Code):

   ```text
   /plugin marketplace add A79-ai/ampup-memory
   /plugin install ampup-memory@ampup-memory
   ```

4. Check it: `/ampup-memory:status` in Claude Code, or `ampup-memory-client doctor` in a terminal.

The plugin's launcher (`plugins/ampup-memory/bin/ampup-memory-client`) downloads a client from a releases repository when
none is installed, and checks once a day for a newer one. With a client built from source, set
`AMPUP_MEMORY_CLIENT_UPDATE=off` to keep it.

### What the hooks do

- **A session per Claude Code session.** SessionStart opens an AmpUp Memory session and puts its id in context.
  Compaction and resume continue it. Stop sends the conversation so far, at most every five minutes, without
  holding up the turn.
- **Advice at the first prompt.** The prompt hook asks `get_advice` for the task, fitted to its share of the
  context (`max_chars`), and adds the answer ahead of the recalled facts with an instruction to follow it unless
  the evidence contradicts it. The call runs in the background; the advice reaches Claude with the next hook.
- **Advice at each step.** After a turn with tool calls, or a tool call that usually ends a step (a commit, a push,
  a build or test run), the hook asks for the `next` moment. The note already served is re-read (no model call) and
  passed on only if a decisions model finds it useful now (`AMPUP_MEMORY_ADVICE_USEFUL_MIN`, 0.4). At most one ask
  per 30 seconds and 12 pieces of advice per session.
- **Recall.** Facts the memory held about the repo are added at session start; the first prompt's best-matching
  facts come with the advice.
- **Ended with its trace.** SessionEnd ends the session with a trace (each prompt, each answer shortened, each tool
  call with shortened arguments and the first 200 characters of its result, under 20,000 characters) and Claude's
  last answer as the outcome. Only its person can read the trace.
- **Notes.** The session is also remembered as a condensed note every five prompts, before compaction and at the
  end.
- **Transcripts (opt-in).** Where the org's or team's admins turn `transcripts` on (else the server's
  `AMPUP_MEMORY_TRANSCRIPTS=1`), the hooks also keep each Claude Code, Codex and Gemini CLI session's whole transcript,
  for its person alone: each prompt, each answer and each tool call with its input and result (whole up to 64 KB,
  else its first and last 32 KB), never thinking. It goes in parts as the session grows, at most 32 MB a session,
  and is never searched, extracted or read for the team. Forgetting a part forgets every version of it, and a part
  you forgot is never sent again. `get_episode {"document": "transcripts/claude-code/<session>/"}` lists a session's
  parts.
- **Starter memory.** At session start the repo's `CLAUDE.md`, `.claude/CLAUDE.md`, `AGENTS.md`, `GEMINI.md`,
  `.claude/rules` and `.cursor/rules`, and your Claude Code memory files for the repo, are imported into your own
  memory (`?scope=mine`), which no teammate reads. At most 40 files of up to 64 KB; a file is sent again only when
  it changed. `AMPUP_MEMORY_STARTER=off` imports nothing.
- **Never waits on the memory.** A slow or down server costs a session its context, never its time.

Everything the hooks send has secret-looking words redacted (known key prefixes, JWTs, long random strings, the
values of `password=`, `token:` and similar).

### Commands

| Command | What it does |
|---|---|
| `/ampup-memory:login [<server>] [--org <org>]` | Signs in through the browser. |
| `/ampup-memory:outcome good\|bad [why]` | Rewards the session +1 or -1; your words are the reason. |
| `/ampup-memory:remember <text>` | Remembers a note. |
| `/ampup-memory:search <words>` | Searches the memory. |
| `/ampup-memory:brief <name> [refresh]` | Where a subject stands, where the schema summarises it. |
| `/ampup-memory:forget <episode id or words>` | Forgets something you remembered. |
| `/ampup-memory:status` | Whether it all works: server, sign-in, org, what runs there, how the last hook went. |

In a terminal, `ampup-memory-client hook outcome good|bad [why]` rates the last session the hooks opened.

### Variables

| Variable | Default | What it does |
|---|---|---|
| `AMPUP_MEMORY_URL` | the CLI context's server, else `http://127.0.0.1:8080` | Where the memory is. |
| `AMPUP_MEMORY_ORG` | the CLI context's org | The org, with `AMPUP_MEMORY_URL`. |
| `AMPUP_MEMORY_TOKEN` | the CLI's sign-in | A bearer to send instead (an API key, for CI). |
| `AMPUP_MEMORY_SCOPE` | the team's | `mine` keeps sessions, notes and recall in your own memory. |
| `AMPUP_MEMORY_CAPTURE` | on | `off`: no notes, no session trace, no transcript. Advice and recall still work. |
| `AMPUP_MEMORY_RECALL` | first prompt | `every` recalls facts on every prompt. |
| `AMPUP_MEMORY_INSTALL_DIR` | `~/.local/bin` | Where the plugin looks for `ampup-memory-client`. |
| `AMPUP_MEMORY_CLIENT_UPDATE` | on | `off` keeps the installed client. |

An organization can set the plugin, `AMPUP_MEMORY_URL` and `AMPUP_MEMORY_ORG` for everyone in Claude Code's managed
settings (`enabledPlugins`, `env`).

## Codex, Gemini CLI, Cursor and Copilot

```bash
ampup-memory hooks install --harness codex      # or gemini, cursor, copilot; --url <server>; --dry-run first
ampup-memory hooks uninstall --harness codex
```

| Harness | Files | Recall |
|---|---|---|
| codex | `~/.codex/hooks.json`, `[mcp_servers.ampup-memory]` in `~/.codex/config.toml` | session start; advice and facts at the first prompt |
| gemini | `hooks` and `mcpServers` in `~/.gemini/settings.json` | session start; advice and facts at the first prompt |
| cursor | `~/.cursor/hooks.json`, `~/.cursor/mcp.json` | session start only (its prompt hook can't add context) |
| copilot | `~/.copilot/hooks/ampup-memory.json`, `~/.copilot/mcp-config.json` | session start only |

The installer keeps everything else in those files, backs each up before its first change
(`<file>.ampup-memory-backup`), and changes nothing when run twice. Those transcripts are read for text only, so their
traces hold no tool calls.

`ampup-memory setup <server> --org <org> [--harness codex,cursor] [--dry-run]` signs in and sets up every agent it
finds in one go, then runs `ampup-memory doctor`.

## Hosts without hooks

Claude Desktop, ChatGPT and other MCP clients use the server directly: point them at `<server>/mcp`, or run
`ampup-memory mcp` where the host starts a command. The
[`ampup-memory-advice`](../../plugins/ampup-memory/skills/ampup-memory-advice/SKILL.md) skill describes the loop for them; add
it as a skill or put it in the agent's instructions.
