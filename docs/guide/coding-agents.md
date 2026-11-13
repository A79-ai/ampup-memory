# Coding agents

The plugin in [`plugins/ampup-memory`](../../plugins/ampup-memory) closes the learning loop in Claude Code with hooks, so
it never depends on the model choosing to call a tool. The same hooks install into Codex, Gemini CLI, Cursor and
Copilot with one command.

## Claude Code

1. Nothing to build: the plugin runs `ampup-memory-client`, a small binary (sign-in, hooks, MCP bridge), and
   downloads it from the [public releases](https://github.com/A79-ai/ampup-memory/releases) when none is installed.
   To install it ahead of time, with the server:

   ```bash
   uv tool install ampup-memory-cli    # or pipx install ampup-memory-cli
   ```

   or with the installer (`AMPUP_MEMORY_INSTALL=client` for the client alone):

   ```bash
   curl -fsSL https://raw.githubusercontent.com/A79-ai/ampup-memory/main/install.sh | sh
   ```

   A client built from source (`cargo install --locked --path crates/memstore-client --root ~/.local`) also works;
   the plugin looks in `~/.local/bin` (where uv and pipx put commands too) first, then your `PATH`.

2. Pick the server. With no server named and no sign-in, the memory is a local server on `http://127.0.0.1:8080`,
   and you don't start it: a session's start runs `ampup-memory serve --local --background` when nothing answers
   there, detached, and the session goes on at once (its memory is in `~/.ampup-memory`, its log in
   `~/.ampup-memory/logs/serve.log`). Sessions starting together start one server. It runs until you stop it or the
   machine restarts. `AMPUP_MEMORY_AUTOSTART=0` turns this off.

   That needs the server, `ampup-memory`, which the plugin doesn't download (it fetches only the client): step 1's
   `uv tool install ampup-memory-cli` or installer puts it in place. Without it, a session's start says how to
   install it.

   It learns with the model kept in `~/.ampup-memory/llm.json`, readable by you alone, as `service install` keeps
   it. Where none is saved yet, the first start saves the one the agent's environment names (`OPENROUTER_API_KEY`,
   `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or `AMPUP_MEMORY_LLM*`, in `service install`'s order) and the session's
   start says which, never the key. A model saved before is never replaced, and the server itself never gets a key
   in its environment. With no key in the environment it keeps and searches memory but learns nothing, and the
   session's start says so: `OPENROUTER_API_KEY=... ampup-memory service save-model` saves one for the server's
   next start. `OPENROUTER_API_KEY=... ampup-memory service install` keeps the server always on, at login too
   (`ampup-memory service status` checks it, `ampup-memory service uninstall` stops it). To run it in a terminal
   instead: `AMPUP_MEMORY_EXTRACT_GOAL_BLIND=1 OPENROUTER_API_KEY=... ampup-memory serve --local`.

   For a team server, sign in once: `ampup-memory-client join https://memory.example.com --org acme`. With a team
   server or `AMPUP_MEMORY_URL` named, nothing is started.

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
- **Rated with one keystroke.** Type `+` or `-` as a prompt (then Enter) to say the session went well or badly. The
  prompt hook records it as your rating of the session and blocks the prompt, so it never reaches Claude and costs
  no turn; you see one line saying it was recorded. A later `+` or `-` replaces it. Your rating is the strongest
  signal advice learns from (after what became of the session's pull requests): it counts as the person's word, and
  the judge's reading never replaces it. `/ampup-memory:good [why]` and `/ampup-memory:bad [why]` do the same with
  your reason. A `+` or `-` with words after it (`- fix the test`) is an ordinary prompt.
- **Asked once.** After the first turn of a session that made five tool calls or more, Stop shows you (not Claude)
  one line saying `+` or `-` rates it, unless you already did. Never again in that session, never in `claude -p` or
  an SDK's session. `AMPUP_MEMORY_QUICK_RATE=quiet` hides the line; `off` also lets a bare `+` or `-` through to
  Claude as a prompt.
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
- **Advice from the first session.** Once a day per repository, the session's start sends the repository's agent
  instructions, guides, docs, your Claude Code memory files for it and its last 25 merged pull requests (through
  `gh`, else recent commits) to your own memory, and the server writes notes for the usual coding tasks from them,
  so `get_advice` has something to serve before any session is rewarded. The first time, the session says what was
  read. Never code, only files git tracks. `AMPUP_MEMORY_REPO_ADVICE=off` sends nothing
  ([advice.md](advice.md#advice-from-a-repository-from-the-first-session)).
- **Never waits on the memory.** A slow or down server costs a session its context, never its time.

Everything the hooks send has secret-looking words redacted (known key prefixes, JWTs, long random strings, the
values of `password=`, `token:` and similar).

### Commands

| Command | What it does |
|---|---|
| `/ampup-memory:login [<server>] [--org <org>]` | Signs in through the browser. |
| `+` or `-` | Rates the session good or bad (typed alone as a prompt; never sent to Claude). |
| `/ampup-memory:good [why]`, `/ampup-memory:bad [why]` | The same, with your reason. |
| `/ampup-memory:outcome good\|bad [why]` | The same, as one command. |
| `/ampup-memory:remember <text>` | Remembers a note. |
| `/ampup-memory:search <words>` | Searches the memory. |
| `/ampup-memory:brief <name> [refresh]` | Where a subject stands, where the schema summarises it. |
| `/ampup-memory:forget <episode id or words>` | Forgets something you remembered. |
| `/ampup-memory:status` | Whether it all works: server, sign-in, org, what runs there, how the last hook went. |

The prompt hook records a rating command before it expands, so none of them costs a turn. In a terminal,
`ampup-memory-client hook outcome good|bad [why]` rates the last session the hooks opened.

### Variables

| Variable | Default | What it does |
|---|---|---|
| `AMPUP_MEMORY_URL` | the CLI context's server, else `http://127.0.0.1:8080` | Where the memory is. |
| `AMPUP_MEMORY_ORG` | the CLI context's org | The org, with `AMPUP_MEMORY_URL`. |
| `AMPUP_MEMORY_TOKEN` | the CLI's sign-in | A bearer to send instead (an API key, for CI). |
| `AMPUP_MEMORY_SCOPE` | the team's | `mine` keeps sessions, notes and recall in your own memory. |
| `AMPUP_MEMORY_CAPTURE` | on | `off`: no notes, no session trace, no transcript. Advice and recall still work. |
| `AMPUP_MEMORY_RECALL` | first prompt | `every` recalls facts on every prompt. |
| `AMPUP_MEMORY_QUICK_RATE` | on | `quiet`: `+` and `-` still rate the session, but Stop never says so. `off`: neither; a bare `+` or `-` goes to Claude. |
| `AMPUP_MEMORY_REPO_ADVICE` | on | `off`: a session's start sends no repository's docs and pull requests for first-session advice. |
| `AMPUP_MEMORY_INSTALL_DIR` | `~/.local/bin` | Where the plugin looks for `ampup-memory-client`, and a session's start for `ampup-memory`. |
| `AMPUP_MEMORY_AUTOSTART` | on | `0`: a session's start never starts the local server. |
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

In Codex, a bare `+` or `-` typed as a prompt rates the session as in Claude Code, and Stop says so once, after a
turn of real work. Gemini CLI, Cursor and Copilot have no way to keep a prompt from the model, so there you rate the
last session from a terminal: `ampup-memory-client hook outcome good|bad [why]`.

`ampup-memory setup <server> --org <org> [--harness codex,cursor] [--dry-run]` signs in and sets up every agent it
finds in one go, then runs `ampup-memory doctor`.

## Hosts without hooks

Claude Desktop, ChatGPT and other MCP clients use the server directly: point them at `<server>/mcp`, or run
`ampup-memory mcp` where the host starts a command. The
[`ampup-memory-advice`](../../plugins/ampup-memory/skills/ampup-memory-advice/SKILL.md) skill describes the loop for them; add
it as a skill or put it in the agent's instructions.
