# The AmpUp Memory plugin

One plugin for Claude Code, Cursor, Codex and ChatGPT. Each host reads its own manifest, and they all share the
skills in [`skills/`](skills):

| | Claude Code | Cursor | Codex | ChatGPT |
|---|---|---|---|---|
| Manifest | `.claude-plugin/plugin.json` | `.cursor-plugin/plugin.json` | `plugin.json` (Agent Plugins 1.0) | `plugin.json` (the same) |
| MCP server | `ampup-memory-client mcp` (stdio, signed in as the CLI is; `ampup-memory mcp` where only AmpUp Memory is installed) | `ampup-memory mcp` | `mcp.json`'s server: a local one by default, a team's over HTTPS with OAuth sign-in | a team's server over HTTPS, OAuth sign-in |
| Hooks | every session event, below | session start, stop, compaction, end | through `ampup-memory hooks install --harness codex` | none |
| Commands | `/ampup-memory:login`, `good`, `bad`, `outcome`, `remember`, `search`, `brief`, `forget`, `status`; `+` or `-` alone rates the session | `ampup-memory-search`, `ampup-memory-remember`, `ampup-memory-brief` | none | none |

The skills:

- [`ampup-memory-advice`](skills/ampup-memory-advice/SKILL.md): run the conversation as an AmpUp Memory session (start,
  advice per task, end with the outcome) so the memory advises on the task and learns whether its advice helped.
- [`ampup-memory-recall`](skills/ampup-memory-recall/SKILL.md): read the memory before guessing, answer with each
  claim's source, apply the user's stored preferences, read where something stands from its summary
  (`get_brief`) where the schema summarises it, and say plainly when the memory has no answer.
- [`ampup-memory-remember`](skills/ampup-memory-remember/SKILL.md): write what lasts: the user's preferences and
  corrections, decisions and procedures, for them alone, without being asked (AmpUp Memory shares one with the team
  once at least three people have said the same); never ephemeral chat or secrets. Forget with confirmation.

As with mem0's and Zep's plugins, the skills are about using memory, not about one domain: which subjects get a
summary, and what it scores, is the team's schema's choice.

## In Claude Code

This plugin connects Claude Code to AmpUp Memory. Each session starts with what the memory has learned about the task, and each session teaches the memory something when it ends:

- **Each Claude Code session is an AmpUp Memory session.** It opens when the session starts (`start_session`), and the session id is added to Claude's context. A compaction or a resume continues it (a session opened for a resumed conversation traces only its new turns), and each Stop sends it the conversation so far, at most every five minutes and without holding up the turn, so a session longer than the server's idle time keeps its trace.
- **Advice at the first prompt.** On a session's first prompt, the hook asks for advice on that task in the session (`get_advice`): the skill the memory has for this kind of task, learned from past sessions and runs (or, before any taught one, compiled from the org's documents), named with where it came from. If the memory has some, it is added to Claude's context first, fitted to its share of the context (`max_chars`), with an instruction to follow it unless the evidence contradicts it. The notes you and your team asked to be remembered that bear on the task come with it (a decisions model picks them, else the ones sharing a word with the task); the session's start hands none, since it comes before the task is known. The hook calls it every time, so it doesn't depend on Claude choosing to call a tool. The hook never waits for it: the call runs in the background, and the advice reaches Claude with the session's next hook, usually its first tool call. The `ampup-memory-advice` skill tells Claude to ask again whenever it needs help and each time a step finishes and it moves to the next (`moment: next`).
- **Advice at each step.** The hook doesn't wait for Claude to ask. After a turn that made tool calls, and after a tool call that usually ends a step (a commit, push, build or test run, a task ticked off), it asks `get_advice` for the `next` moment: the note the session was served is re-read for it (no model call), and it reaches Claude only if a decisions model finds it relevant and useful to what Claude is doing (`AMPUP_MEMORY_ADVICE_USEFUL_MIN`, 0.4), and the session hasn't been told it already. One ask per 30 seconds at most, and at most 12 pieces of advice per session.
- **One budget for what it adds** (from 0.4.12). At a session's start and its first prompt the hooks add at most 11,000 characters in all (about 2,750 tokens). The repo's facts at the start come before the task is known, so they get a share of 1,500 (about four facts). The first prompt's are handed on by one hook, and Claude Code takes at most 10,000 characters a hook, so they take at most 9,500, in this order: the advice for the task (at most 7,500, as before), the notes you asked to be remembered that bear on it, then your team's that do (newest first, at most 1,200 together), the session's line and the leads (up to 800), then the facts that best match the task in what is left (at most 4,000). What AmpUp Memory adds is read in every turn of the session, and a note about other work is one more instruction to weigh, so it adds what bears on the task, and little of it. What comes later (advice at a step's end or at a tool call) comes one piece at a time, apart from it.
- **Recalled at the start.** When a session starts, the facts the memory held about the repo when last asked are added to Claude's context at once (their share of the budget), and refreshed in the background for the next session. The first prompt's facts that best match it come with the advice. A first prompt longer than 600 characters is searched for its start and end, your own words before anything pasted (fenced code, a pasted block), rather than all of it.
- **Never waits on the memory.** No hook holds up Claude or you: what a hook needs from the memory is fetched by a background copy of it, and the session's next hook hands it on. A slow or down memory costs a session its context, never its time.
- **Ended with its trace.** When the session ends, the hook ends the AmpUp Memory session (`end_session`) with its trace and outcome (Claude's last answer). The trace lists each prompt, each answer (shortened) and each tool call with its arguments (shortened) and the first 200 characters of its result. The server judges whether the advice served in it was applied and helped, and a rated session teaches advice like a recorded run. Claude Code gives SessionEnd hooks 1.5 seconds, so the hook hands this to a copy of itself that runs on (failures go to `log-session-end` in the hooks' directory).
- **Rated with one keystroke.** Type `+` or `-` alone as a prompt to say the session went well or badly. The prompt hook records it as your rating and blocks the prompt, so Claude never sees it and it costs no turn; you see one line saying so, and a later `+` or `-` replaces it. `/ampup-memory:good it fixed the flaky test` or `/ampup-memory:bad wrong file` does the same with your reason (as does `/ampup-memory:outcome good|bad [why]`), also without a turn. Typed first thing after `claude --resume`, it rates the work the conversation resumed. Your rating is the strongest signal the memory learns from after what became of the session's pull requests: it counts as your word, which the judge's reading never replaces, and your reason is what a note's pitfall is drawn from. A `+` or `-` with words after it (`- fix the test`) is an ordinary prompt.
- **Asked once.** After the first turn of a session that made five tool calls or more, Stop shows you (not Claude) one line: `+` or `-` rates the session. Never again in that session, never once you rated it, and never in `claude -p` or an SDK's session. `AMPUP_MEMORY_QUICK_RATE=quiet` hides it; `off` also lets a bare `+` or `-` through to Claude.
- **Remembered as you go.** A session is also remembered as a note before it compacts, every five prompts, and when it ends. The note is condensed to what was asked, what Claude said and the files it changed, without tool output or thinking. With goal-blind extraction on (`AMPUP_MEMORY_EXTRACT_GOAL_BLIND=1`), the server turns it into facts. A session no schema `extract` section names extracts nothing by AmpUp Memory's default. A session remembered again as it grows adds only what is new.
- **Kept whole, where your org asks.** Where the org's or team's admins turn `transcripts` on, the hooks also keep the session's whole transcript, for you alone: each prompt, each answer, each tool call with its input and its result (whole up to 64 KB, past that its first and last 32 KB), never thinking, and never the memory's own text: what AmpUp Memory's tools returned and the recall its hooks added are left out, a reference to the memories they named in their place, so those keep their own access rules. It goes in parts before a compaction, at most every five minutes at a Stop, and when the session ends, each part only what the transcript gained since the last, so a later pass can check what was noted against what actually ran. Nothing reads it for the team, nor is it searched or turned into facts; `/ampup-memory:forget` (every version of a part at once) and forgetting you remove it like your notes, and a part you forgot is never sent again; your org's retention can age transcripts out on their own (`memstore:transcript:=30d`). `get_episode` with `document` `transcripts/claude-code/<session id>/` lists a session's parts. Claude Code, Codex and Gemini CLI; at most 32 MB a session.
- **Starter memory.** When a session starts, the repo's checked-in `CLAUDE.md`, `AGENTS.md`, `GEMINI.md` and rules
  (`.claude/rules`, `.cursor/rules`) and your Claude Code memory files for the repo are imported into your own
  memory, which only you read, so a repo you don't trust can't put its instructions in your teammates' sessions.
  A file is sent again only when it changed. The server turns them into facts even where nothing else is
  extracted, so your first sessions already recall something.
- **Commands.** `/ampup-memory:login [<server>] [--org <org>]` signs in, `+` or `-` (or `/ampup-memory:good [why]`, `/ampup-memory:bad [why]`, `/ampup-memory:outcome good|bad [why]`) rates the session, `/ampup-memory:remember <text>` remembers a note, `/ampup-memory:search <words>` searches the memory, `/ampup-memory:brief <name> [refresh]` says where something stands from its summary, `/ampup-memory:forget <episode id or words>` forgets something you remembered, and `/ampup-memory:status` (`ampup-memory doctor` in a terminal) says whether the memory works here: where it is and whether it's up, who you're calling as and until when, the org, what the memory runs there (extraction, advice learning and its evidence bar), and how the last hook went, with any warning it had.
- **Tools.** The AmpUp Memory MCP server's tools are available to Claude: by default `start_session`, `get_advice`, `end_session`, `search_memory`, `get_episode`, `remember_memory` and `forget_memory`, plus `get_brief` and `list_briefs` where the schema summarises its subjects, and the rest with `AMPUP_MEMORY_MCP_TOOLS=all` on the server.

Words that look like secrets are redacted from everything the hooks send: notes, session traces, transcripts, prompts and outcomes. That covers known key prefixes, JWTs, long random-looking strings, and the value of `password=`, `token:` and the like, including inside JSON and YAML.

## Install

The plugin is in the public [A79-ai/ampup-memory](https://github.com/A79-ai/ampup-memory) repository, whose root is
its marketplace. `ampup-memory-client`, the binary it runs (the sign-in, the hooks and the MCP bridge, a few MB, for
macOS on Apple Silicon or Intel and Linux on arm64 or x86_64), is published in that repository's GitHub releases, as
the `client-v*` tags (the server's are `v*`):

```text
/plugin marketplace add A79-ai/ampup-memory
/plugin install ampup-memory@ampup-memory
/ampup-memory:login https://memory.example.com --org acme
```

With no team server, skip the sign-in: the plugin uses a local server, `http://127.0.0.1:8080`, when nothing else is
set, and starts it when a session starts (below).

You don't install the binary yourself. Everything the plugin runs goes through
[`bin/ampup-memory-client`](bin/ampup-memory-client), which runs `ampup-memory-client` from `~/.local/bin` or your `PATH`
(never a full `ampup-memory` instead, which may be older than the plugin). Where it isn't installed, it installs the newest client release,
checking its checksum ([`bin/install.sh`](bin/install.sh)). A session's start installs it in the background and
tells Claude to say once that memory is on from the next session. The other hooks do nothing until then, so a turn
never waits on a download. The MCP server and the commands install it first and then run.

`/ampup-memory:login` signs in through the browser once (`ampup-memory-client join`) as you, with Google or a password, and
the CLI renews the sign-in. The server then says which orgs hold you: one, and you're in it; several, and
`/ampup-memory:login --org <org>` names one; none yet, and `/ampup-memory:login --org <org>` asks to join that org: its admins
are emailed a link that approves you (or they run `ampup-memory requests` and `ampup-memory invite`), and the next
`/ampup-memory:login` gets in. With no server named it signs in to `AMPUP_MEMORY_URL`; with neither, it says the memory is the local server, which
needs no sign-in. `AMPUP_MEMORY_ORG` names the org for everyone. An organization can set the server and org for everyone, together with
the plugin itself, in Claude Code's managed settings:

```json
{
  "extraKnownMarketplaces": {
    "ampup-memory": { "source": { "source": "github", "repo": "A79-ai/ampup-memory" }, "autoUpdate": true }
  },
  "enabledPlugins": { "ampup-memory@ampup-memory": true },
  "env": { "AMPUP_MEMORY_URL": "https://memory.example.com", "AMPUP_MEMORY_ORG": "acme" }
}
```

While nobody is signed in on a machine, a session's start tells Claude to say once that `/ampup-memory:login` turns
memory on. `/ampup-memory:status` (`ampup-memory-client doctor` in a terminal) checks it all.

To install the binary by hand (for Cursor, a terminal, or CI):

```bash
curl -fsSL https://raw.githubusercontent.com/A79-ai/ampup-memory/main/plugins/ampup-memory/bin/install.sh | sh   # to ~/.local/bin
ampup-memory-client join https://memory.example.com --org acme
```

`ampup-memory setup <server> --org <org>`, from the full `ampup-memory` install, signs in and sets up the other coding agents
it finds as well.

With no team server named and nobody signed in, the memory is a local server on `http://127.0.0.1:8080`, and you
don't start it: a session's start (or the MCP server, if Claude Code starts it first) runs
`ampup-memory serve --local --background` when nothing answers there, detached, so the session never waits on it.
Sessions starting together start one. It keeps the memory in `~/.ampup-memory`, logs to
`~/.ampup-memory/logs/serve.log`, and runs until you stop it or the machine restarts. `AMPUP_MEMORY_AUTOSTART=0`
turns this off. The plugin downloads only the client, never the server (`ampup-memory`, a much larger download): without it, a
session's start says how to install it:

```bash
uv tool install ampup-memory-cli    # or: curl -fsSL https://raw.githubusercontent.com/A79-ai/ampup-memory/main/install.sh | sh
```

It learns with the model kept in `~/.ampup-memory/llm.json`, readable by you alone. Where none is saved yet, the first
start saves the one Claude Code's environment names (`OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`, `OPENAI_API_KEY` or
`AMPUP_MEMORY_LLM*`), as `ampup-memory service install` does, and the session's start says which (never the key). A
saved model is never replaced, and the server never gets a key in its environment. With no key there, it keeps and
searches memory but learns nothing, and the session's start says so: `ampup-memory service save-model` with a key
set saves one for the server's next start. To keep the server always on, at login too:

```bash
OPENROUTER_API_KEY=... ampup-memory service install   # ampup-memory service status, ampup-memory service uninstall
```

| Variable | Default | What it does |
|---|---|---|
| `AMPUP_MEMORY_URL` | the CLI context's server, else `http://127.0.0.1:8080` | Where the memory is, and the server `/ampup-memory:login` signs in to. |
| `AMPUP_MEMORY_ORG` | the CLI context's org | The org, with `AMPUP_MEMORY_URL`. |
| `AMPUP_MEMORY_TOKEN` | the CLI's sign-in to that server | A bearer token to send to `AMPUP_MEMORY_URL` instead (an API key, for CI); ignored without `AMPUP_MEMORY_URL`. |
| `AMPUP_MEMORY_SCOPE` | the team's | `mine` keeps sessions, notes and recall in your own memory, which only you can read. |
| `AMPUP_MEMORY_CAPTURE` | on | Set it to `off` to stop sessions being remembered: no trace when they end, and the server keeps nothing you say in them as your memory (from 0.4.6). Advice and recall still work. |
| `AMPUP_MEMORY_RECALL` | first prompt | `every` recalls facts on every prompt. Advice comes on the first prompt only. |
| `AMPUP_MEMORY_QUICK_RATE` | on | `quiet`: `+` and `-` still rate the session, but Stop never says so. `off`: neither; a bare `+` or `-` goes to Claude. The rating commands work either way. |
| `AMPUP_MEMORY_INSTALL_DIR` | `~/.local/bin` | Where the plugin looks for `ampup-memory-client` and installs it, and for `ampup-memory`, the local server. |
| `AMPUP_MEMORY_AUTOSTART` | on | `0`: a session's start never starts the local server. |
| `AMPUP_MEMORY_CLIENT_UPDATE` | on | Once a day a session's start checks, in the background, whether a newer `ampup-memory-client` was released and installs it. `off` keeps the one installed; a set `AMPUP_MEMORY_VERSION` pins it. |

## Moving from memstore

The plugin used to be called `memstore` (`memstore@memstore`, from the `memstore` marketplace). Its last version
says so once at a session's start, and `/memstore:switch` there makes the move for you. By hand, in Claude Code:

```text
/plugin marketplace add A79-ai/ampup-memory
/plugin install ampup-memory@ampup-memory
/plugin uninstall memstore@memstore
/plugin marketplace remove memstore
```

Then restart Claude Code. The sign-in carries over: the client moves `~/.memstore` to `~/.ampup-memory` on its first
run and leaves a link, and `memstore-client` stays installed as a link to `ampup-memory-client`. Not signed in yet:
`/ampup-memory:login <server> --org <org>`. The commands are `/ampup-memory:*` now, the skills `ampup-memory-*`, and
the settings `AMPUP_MEMORY_*` (each still read under its old `MEMSTORE_*` name when the new one isn't set). Keep one
of the two plugins: while `ampup-memory` is enabled, the old plugin's last version does nothing, so no session is
recorded twice. In Cursor, install `ampup-memory` from this repository's marketplace and remove `memstore`.

Where an organization rolls the plugin out in Claude Code's managed settings, its admin moves everyone in one edit:
add the `ampup-memory` marketplace, set `enabledPlugins` to `{"ampup-memory@ampup-memory": true,
"memstore@memstore": false}` (false, so the two never both run), and set `AMPUP_MEMORY_URL` and `AMPUP_MEMORY_ORG`
beside the old `MEMSTORE_URL` and `MEMSTORE_ORG`, with the same values, until every machine has moved.

## In Cursor

The plugin gives Cursor the skills, three commands, the hooks and the MCP server. Like Claude Code, it needs the
`ampup-memory-client` binary on your `PATH` (the `install.sh` above) and a sign-in (`ampup-memory-client join <server> --org <org>`). Cursor's prompt hook can't
add context, so a session gets what the memory knows at its start, but no advice at its first prompt: the
`ampup-memory-advice` skill asks for it instead. Install it from this repository in Cursor's plugin settings
(`.cursor-plugin/marketplace.json` lists it). Use the plugin or `ampup-memory hooks install --harness cursor`, not
both, or every session's hooks run twice.

## In Codex

Codex reads the portable manifest (`plugin.json`, `mcp.json`, `skills/`):

```bash
codex plugin marketplace add A79-ai/ampup-memory  # .agents/plugins/marketplace.json
codex plugin add ampup-memory@ampup-memory
codex mcp login ampup-memory                      # OAuth sign-in, once mcp.json names the team's server
```

`mcp.json` names a local server (`http://127.0.0.1:8080/mcp`, `ampup-memory serve --local`), which needs no sign-in;
a team running its own server changes that URL. The plugin declares no hooks (Codex runs a plugin's hooks only in
its desktop app, after you trust them). For
sessions recorded as runs, advice at the first prompt and rating a session with `+` or `-`, also run `ampup-memory hooks install --harness codex`.

## In ChatGPT

ChatGPT reads the same portable manifest. A workspace admin imports the plugin from this repository (**Admin** >
**Plugins**) and chooses who gets it. Each person signs in to the team's server with OAuth when they first use it.
ChatGPT runs no hooks, so the `ampup-memory-advice` skill opens and ends each session. ChatGPT can't reach a server on
your machine, so the workspace's copy of `mcp.json` must name the team's server over HTTPS instead of the local one.

Without hooks, what the user tells the agent outlasts the conversation only if it's written down. Two things make
sure it is. The `ampup-memory-remember` skill and the server's instructions ask the agent to call `remember_memory` in
the turn a preference, correction, decision or procedure is stated. The server also keeps these from the user's
words in the session calls, writing each once (AmpUp Memory's README, MCP tools, "Kept from what the user
said"). Measured on Claude Code with every hook off, three runs per prompt and arm: Haiku 5.5 kept 18 of 18 and
Sonnet 5.5 15 of 18. The misses were preferences where the agent called no tool at all. A "thanks, that's all"
kept nothing in any run.

## Notes

- **Who can read what.** A session's trace is readable by the person whose session it was alone, never by teammates. Advice learning reads every rewarded session and run in the scope, and only an org's admins start it or turn it on. A remembered session note is yours alone too: it holds what you said, which reaches teammates only once at least three people have said the same (the server's keep and share, in AmpUp Memory's README). On `serve --local` that is just you.
- **A session is a person's.** With token auth, sessions are kept under your own token. A service key can't keep sessions, so with one the server refuses them and the hook says so on stderr. The session is still remembered if the key may write.
- **Finding the session to rate.** `+`, `-` and the rating commands get the session id from Claude Code. Run by hand in a terminal (`ampup-memory-client hook outcome good "why"`), it rates the session the hooks opened last. The hooks keep each session's AmpUp Memory id in an owner-only directory under your temp directory.
- **The transcript format can change.** The session's trace and its note come from Claude Code's transcript file, whose format is internal and can change between releases. Lines the hook can't read are skipped, so a change loses detail rather than breaking the session.
- **Hosts without hooks.** Other MCP clients (Claude Desktop, Cursor and others) can use the same server without the hooks: run `ampup-memory mcp` (signed in as the CLI is) where the host starts a command, or point it at `<server>/mcp`. The [`ampup-memory-advice`](skills/ampup-memory-advice/SKILL.md) skill describes the loop for them: `start_session` first, `get_advice` when a task starts, `end_session` with a condensed transcript and the outcome at the end. Add it as a skill where the host takes skills, or put it in the agent's instructions.
- **Codex, Gemini CLI, Cursor and Copilot.** `ampup-memory hooks install --harness <name>` registers the same hooks. Codex and Gemini get the advice at the first prompt; Cursor and Copilot can't add context at a prompt, so they get no advice. All four end their session with its trace when it ends. Their traces hold what was said, without tool calls, because those transcripts are read for text only. In Codex, `+` or `-` typed alone rates the session as in Claude Code. Elsewhere, rate the last session from a terminal with `ampup-memory hook outcome good|bad [why]`.
- **Benchmarks.** How the learning loop and memory were measured on public benchmarks, and the results: [benchmarks](https://github.com/A79-ai/ampup-memory/blob/main/benchmarks/README.md).
