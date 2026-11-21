# AmpUp Memory for OpenCode and Kilo Code

[`ampup-memory.ts`](ampup-memory.ts) is an OpenCode plugin (the v1 plugin API, `opencode-ai` 1.x) that gives each
session the team memory the way the Claude Code plugin does. Kilo Code's CLI (from v7) is built on OpenCode and loads
the same file; the plugin sends `--harness kilo` there and `--harness opencode` in OpenCode.

| OpenCode | Runs | What it does |
|---|---|---|
| `session.created` (not a subagent's) | `ampup-memory-client hook session-start` | opens an AmpUp Memory session; what it adds goes in the system prompt |
| `chat.message` | `… hook prompt` | asks for advice on the prompt; what it adds goes in the system prompt |
| `session.idle` | `… hook stop` | sends the session so far |
| `experimental.session.compacting` | `… hook session-end` | remembers the session before it is compacted |
| `session.deleted`, OpenCode exiting | `… hook end` | ends the AmpUp Memory session |

What the hooks add reaches the model through `experimental.chat.system.transform`: the session's start, then each
prompt's, at most 12,000 characters, newest first. Each hook gets Claude Code's hook input on stdin, with
`transcript_path` a transcript in Claude Code's format that the plugin keeps from the session's messages, under
`~/.cache/ampup-memory/transcripts/` (OpenCode's tool calls under Claude Code's names, so the memory sees which files
were edited). A hook that fails or takes too long (10 seconds for a prompt) is skipped; the session never waits on it.

## Install

1. Install the client and sign in:

   ```sh
   curl -fsSL https://raw.githubusercontent.com/A79-ai/ampup-memory/main/plugins/ampup-memory/bin/install.sh | sh
   ~/.local/bin/ampup-memory-client join https://memory-api.ampup.ai --org <your org>
   ```

   (`--api-key <key>` signs in without a browser.)
2. Put the plugin where the agent loads it, either with `ampup-memory-client hooks install --harness opencode` (or
   `--harness kilo`), or by hand:

   ```sh
   # OpenCode
   mkdir -p ~/.config/opencode/plugins
   curl -fsSL -o ~/.config/opencode/plugins/ampup-memory.ts https://raw.githubusercontent.com/A79-ai/ampup-memory/main/plugins/integrations/opencode/ampup-memory.ts
   # Kilo Code
   mkdir -p ~/.config/kilo/plugin
   curl -fsSL -o ~/.config/kilo/plugin/ampup-memory.ts https://raw.githubusercontent.com/A79-ai/ampup-memory/main/plugins/integrations/opencode/ampup-memory.ts
   ```

3. Restart OpenCode (or Kilo Code).

The plugin adds hooks only, not the memory's MCP tools: add the MCP server to `opencode.json` (or `kilo.json`) as the
setup page says.

Settings: `AMPUP_MEMORY_CLIENT` (the client to run; default `~/.local/bin/ampup-memory-client`, else the PATH's),
`AMPUP_MEMORY_HARNESS` (the harness name to send), `AMPUP_MEMORY_HOOKS=off` (the plugin does nothing).

## Versions

- OpenCode `opencode-ai` 1.19.6: loads it from `~/.config/opencode/plugins/`; every event above fired in a real
  session but the compaction, which wasn't triggered.
- Kilo Code `@kilocode/cli` 7.8.32: loads it from `~/.config/kilo/plugin/` (or `plugins/`) and fires the same events.
- OpenCode 2 (`@opencode/cli` 2.x) has a different plugin API and doesn't load this file: no plugin is found and no
  hook runs. A port to it is still to do.
