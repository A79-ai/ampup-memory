# AmpUp Memory for Amp

[`ampup-memory.ts`](ampup-memory.ts) is an Amp plugin (Amp's Plugin API) that gives each Amp thread the team memory
the way the Claude Code plugin does:

| Amp | Runs | What it does |
|---|---|---|
| `session.start` | `ampup-memory-client hook session-start --harness amp` | opens an AmpUp Memory session for the thread (a thread switched away from ends its own) |
| `agent.start` | `… hook prompt --harness amp` | asks for advice on the prompt; what the start and the prompt add rides with the prompt, hidden in the thread's view |
| `agent.end` | `… hook stop --harness amp` | adds the turn to the transcript and sends the session so far |
| Amp exiting (`onDispose`) | `… hook end --harness amp` | ends the AmpUp Memory session |

Amp has no compaction event, so there is no `session-end` before one. Each hook gets Claude Code's hook input on
stdin, with `transcript_path` a transcript in Claude Code's format that the plugin keeps from each turn's messages,
under `~/.cache/ampup-memory/transcripts/` (Amp's `edit_file` and `create_file` as Claude Code's `Edit` and `Write`,
and without the context the plugin added to the prompt). The plugin runs the client with `node:child_process`, not
`amp.$`, which can't take stdin or a timeout. A hook that fails or takes too long (10 seconds for a prompt) is
skipped; the thread never waits on it.

## Install

1. Install the client and sign in:

   ```sh
   curl -fsSL https://raw.githubusercontent.com/A79-ai/ampup-memory/main/plugins/ampup-memory/bin/install.sh | sh
   ~/.local/bin/ampup-memory-client join https://memory-api.ampup.ai --org <your org>
   ```

   (`--api-key <key>` signs in without a browser.)
2. Put the plugin where Amp loads it, with `ampup-memory-client hooks install --harness amp`, or by hand:

   ```sh
   mkdir -p ~/.config/amp/plugins
   curl -fsSL -o ~/.config/amp/plugins/ampup-memory.ts https://raw.githubusercontent.com/A79-ai/ampup-memory/main/plugins/integrations/amp/ampup-memory.ts
   ```

   (`$XDG_CONFIG_HOME/amp/plugins/` where `XDG_CONFIG_HOME` is set; a project's `.amp/plugins/` for one project.)
3. Start a new thread, or run `plugins: reload` from the command palette in a running Amp.

The plugin adds hooks only, not the memory's MCP tools: add the MCP server to Amp's settings as the setup page says.

Settings: `AMPUP_MEMORY_CLIENT` (the client to run; default `~/.local/bin/ampup-memory-client`, else the PATH's),
`AMPUP_MEMORY_HOOKS=off` (the plugin does nothing).

## Verified so far

Amp's CLI loads it from `~/.config/amp/plugins/` and registers `session.start`, `agent.start` and `agent.end` (its
log: "Plugin started"). Its handlers were driven with recorded-shape events, which sent each hook its input and
handed the hooks' context back as the turn's hidden message. A live Amp thread needs an Amp account, and hasn't been
run yet.
