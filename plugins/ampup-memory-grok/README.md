# AmpUp Memory for Grok Bot

A Cursor-format plugin for Grok Bot (and Cursor's cloud agents), which install Cursor plugins and run their hooks.
It gives each Grok Bot session the team memory the way the Claude Code plugin does:

| Grok Bot event | Runs | What it does |
|---|---|---|
| `sessionStart` | `ampup-memory-client hook session-start --harness grok` | opens an AmpUp Memory session and adds what the memory knows about the repo |
| `beforeSubmitPrompt` | `… hook prompt --harness grok` | asks the memory for advice on the prompt, and adds it with the notes that bear on it (`additional_context`) |
| `stop` | `… hook stop --harness grok` | sends the session so far (its trace; remembered every five prompts) |
| `preCompact` | `… hook session-end --harness grok` | remembers the session before the context is compacted |
| `sessionEnd` | `… hook end --harness grok` | ends the AmpUp Memory session with its trace and outcome |

Every hook runs `"${CURSOR_PLUGIN_ROOT}/bin/ampup-memory-client"`, the plugin's launcher, not a client on the PATH:
Grok Bot runs only a plugin's hooks on its cloud computer (a user-level `~/.cursor/hooks.json` there doesn't fire).
The plugin also adds the memory's MCP tools (`search_memory`, `get_advice`, `remember_memory`, …) from AmpUp's hosted
server, `https://memory-api.ampup.ai/mcp`, signed in with OAuth the first time a tool is used, and the skills and
commands of the [AmpUp Memory plugin](../ampup-memory) (`ampup-memory-search`, `ampup-memory-remember`,
`ampup-memory-brief`).

## Install

1. In Grok Bot, add the plugin from its repository link, `https://github.com/A79-ai/ampup-memory` (the repository's
   root is a Cursor marketplace; pick **ampup-memory-grok**), or from the Cursor Marketplace once it is listed there.
2. Sign the cloud computer in to your team's memory, once. It has no browser, so it signs in with a personal API
   key from the AmpUp Memory console:

   ```sh
   ~/.local/bin/ampup-memory-client join https://memory-api.ampup.ai --org <your org> --api-key <your key>
   ```

   Ask Grok Bot to run it in its terminal, or run it yourself. The client is installed by the first session's start
   (below); to sign in before any session, install it first:

   ```sh
   curl -fsSL https://raw.githubusercontent.com/A79-ai/ampup-memory/main/plugins/ampup-memory/bin/install.sh | sh
   ```

3. Start a session. The first prompt's context carries an `AmpUp Memory session:` line when the hooks reach the
   memory; `~/.local/bin/ampup-memory-client doctor` says where it is, who you are and how the last hook went.

## The first session on a fresh computer

Grok Bot's cloud computer may be new for a session, so the launcher doesn't do what it does on a laptop (install in
the background, memory on from the next session). With `--harness grok`, a session's start (or a prompt, where no
start hook ran first) installs `ampup-memory-client` into `~/.local/bin`, checking its checksum, waits for it, then
runs it, so the memory is on from the first session. The install took 2 seconds in tests on fresh Linux x86_64 and
arm64 containers; a start waits for it at most 75 seconds (its hook allows 90), a prompt at most 10 (its hook
allows 15), and a hook that gives up leaves the install running for the next one. The other hooks never wait.

What it needs:

- Linux on x86_64 or arm64 (or macOS). The current Linux build needs glibc 2.38 or newer (Ubuntu 24.04, Debian 13,
  Fedora 39 or newer). On an older system the client installs but can't run: the session's start says so once, and
  the memory stays off there.
- `curl` or `wget`, `tar` and `sha256sum` (or `shasum`).
- GitHub's API, to find the newest client release: 60 lookups an hour from one address without a token, which a
  shared egress address can use up. Set `GITHUB_TOKEN`, or name the release with `AMPUP_MEMORY_VERSION=client-v…`
  (which also stops the client's hourly self-update).

If the install fails, the session's start says so once, with the command to install it by hand, and the session
goes on without the memory.

## Your own server

The hooks use whatever server the client signed in to (`join <server>`), so an org on its own AmpUp Memory server
gets its hooks there. The MCP server in `mcp.json` is fixed to AmpUp's hosted one: Cursor's plugin `variables` are
documented for an MCP server's headers and `env`, not its `url`. An org on its own server adds its MCP server in
Grok Bot's MCP settings (`https://<server>/mcp`) and can leave this plugin's tools unused.

## What isn't verified yet

The manifest and hook shapes are checked against Cursor's plugin reference and Grok Bot's own validators (sessionStart
and beforeSubmitPrompt both take `additional_context`), and the launcher's first-session install against fresh Linux
containers. A live Grok Bot session, with `CURSOR_PLUGIN_ROOT` set for the hooks and the hooks' sends reaching the
server, hasn't been run yet. The `grok` harness and `join --api-key` come with the client release after 0.1.19.
