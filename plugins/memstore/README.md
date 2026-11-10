# The memstore plugin

One plugin for Claude Code, Cursor, Codex and ChatGPT. Each host reads its own manifest, and they all share the
skills in [`skills/`](skills):

| | Claude Code | Cursor | Codex | ChatGPT |
|---|---|---|---|---|
| Manifest | `.claude-plugin/plugin.json` | `.cursor-plugin/plugin.json` | `plugin.json` (Agent Plugins 1.0) | `plugin.json` (the same) |
| MCP server | `memstore-client mcp` (stdio, signed in as the CLI is; `memstore mcp` where only memstore is installed) | `memstore mcp` | `mcp.json`'s server: a local one by default, a team's over HTTPS with OAuth sign-in | a team's server over HTTPS, OAuth sign-in |
| Hooks | every session event, below | session start, stop, compaction, end | through `memstore hooks install --harness codex` | none |
| Commands | `/memstore:login`, `outcome`, `remember`, `search`, `brief`, `forget`, `status` | `memstore-search`, `memstore-remember`, `memstore-brief` | none | none |

The skills:

- [`memstore-advice`](skills/memstore-advice/SKILL.md): run the conversation as a memstore session (start,
  advice per task, end with the outcome) so the memory advises on the task and learns whether its advice helped.
- [`memstore-recall`](skills/memstore-recall/SKILL.md): read the memory before guessing, answer with each
  claim's source, apply the user's stored preferences, read where something stands from its summary
  (`get_brief`) where the schema summarises it, and say plainly when the memory has no answer.
- [`memstore-remember`](skills/memstore-remember/SKILL.md): write what lasts: the user's preferences and
  corrections, decisions and procedures, for them alone, without being asked (memstore shares one with the team
  once at least three people have said the same); never ephemeral chat or secrets. Forget with confirmation.

As with mem0's and Zep's plugins, the skills are about using memory, not about one domain: which subjects get a
summary, and what it scores, is the team's schema's choice.

## In Claude Code

This plugin connects Claude Code to memstore. Each session starts with what the memory has learned about the task, and each session teaches the memory something when it ends:

- **Each Claude Code session is a memstore session.** It opens when the session starts (`start_session`), and the session id is added to Claude's context. A compaction or a resume continues it (a session opened for a resumed conversation traces only its new turns), and each Stop sends it the conversation so far, at most every five minutes and without holding up the turn, so a session longer than the server's idle time keeps its trace.
- **Advice at the first prompt.** On a session's first prompt, the hook asks for advice on that task in the session (`get_advice`): the skill the memory has for this kind of task, learned from past sessions and runs (or, before any taught one, compiled from the org's documents), named with where it came from. If the memory has some, it is added to Claude's context first, fitted to its share of the context (`max_chars`), with an instruction to follow it unless the evidence contradicts it. The notes you and your team asked to be remembered that bear on the task come with it (a decisions model picks them, else the ones sharing a word with the task); the session's start hands none, since it comes before the task is known. The hook calls it every time, so it doesn't depend on Claude choosing to call a tool. The hook never waits for it: the call runs in the background, and the advice reaches Claude with the session's next hook, usually its first tool call. The `memstore-advice` skill tells Claude to ask again whenever it needs help and each time a step finishes and it moves to the next (`moment: next`).
- **Advice at each step.** The hook doesn't wait for Claude to ask. After a turn that made tool calls, and after a tool call that usually ends a step (a commit, push, build or test run, a task ticked off), it asks `get_advice` for the `next` moment: the note the session was served is re-read for it (no model call), and it reaches Claude only if a decisions model finds it relevant and useful to what Claude is doing (`MEMSTORE_ADVICE_USEFUL_MIN`, 0.4), and the session hasn't been told it already. One ask per 30 seconds at most, and at most 12 pieces of advice per session.
- **One budget for what it adds** (from 0.4.12). At a session's start and its first prompt the hooks add at most 11,000 characters in all (about 2,750 tokens). The repo's facts at the start come before the task is known, so they get a share of 1,500 (about four facts). The first prompt's are handed on by one hook, and Claude Code takes at most 10,000 characters a hook, so they take at most 9,500, in this order: the advice for the task (at most 7,500, as before), the notes you asked to be remembered that bear on it, then your team's that do (newest first, at most 1,200 together), the session's line and the leads (up to 800), then the facts that best match the task in what is left (at most 4,000). What memstore adds is read in every turn of the session, and a note about other work is one more instruction to weigh, so it adds what bears on the task, and little of it. What comes later (advice at a step's end or at a tool call) comes one piece at a time, apart from it.
- **Recalled at the start.** When a session starts, the facts the memory held about the repo when last asked are added to Claude's context at once (their share of the budget), and refreshed in the background for the next session. The first prompt's facts that best match it come with the advice. A first prompt longer than 600 characters is searched for its start and end, your own words before anything pasted (fenced code, a pasted block), rather than all of it.
- **Never waits on the memory.** No hook holds up Claude or you: what a hook needs from the memory is fetched by a background copy of it, and the session's next hook hands it on. A slow or down memory costs a session its context, never its time.
- **Ended with its trace.** When the session ends, the hook ends the memstore session (`end_session`) with its trace and outcome (Claude's last answer). The trace lists each prompt, each answer (shortened) and each tool call with its arguments (shortened) and the first 200 characters of its result. The server judges whether the advice served in it was applied and helped, and a rated session teaches advice like a recorded run. Claude Code gives SessionEnd hooks 1.5 seconds, so the hook hands this to a copy of itself that runs on (failures go to `log-session-end` in the hooks' directory).
- **Rated with `/memstore:outcome`.** `/memstore:outcome good it fixed the flaky test` or `/memstore:outcome bad wrong file` rewards the session with +1 or -1, and your words become the reason. Rated sessions teach the memory the most, because they show which steps got the right outcome.
- **Remembered as you go.** A session is also remembered as a note before it compacts, every five prompts, and when it ends. The note is condensed to what was asked, what Claude said and the files it changed, without tool output or thinking. With goal-blind extraction on (`MEMSTORE_EXTRACT_GOAL_BLIND=1`), the server turns it into facts. A session no schema `extract` section names extracts nothing by memstore's default. A session remembered again as it grows adds only what is new.
- **Kept whole, where your org asks.** Where the org's or team's admins turn `transcripts` on, the hooks also keep the session's whole transcript, for you alone: each prompt, each answer, each tool call with its input and its result (whole up to 64 KB, past that its first and last 32 KB), never thinking. It goes in parts before a compaction, at most every five minutes at a Stop, and when the session ends, each part only what the transcript gained since the last, so a later pass can check what was noted against what actually ran. Nothing reads it for the team, nor is it searched or turned into facts; `/memstore:forget` (every version of a part at once) and forgetting you remove it like your notes, and a part you forgot is never sent again. `get_episode` with `document` `transcripts/claude-code/<session id>/` lists a session's parts. Claude Code, Codex and Gemini CLI; at most 32 MB a session.
- **Starter memory.** When a session starts, the repo's checked-in `CLAUDE.md`, `AGENTS.md`, `GEMINI.md` and rules
  (`.claude/rules`, `.cursor/rules`) and your Claude Code memory files for the repo are imported into your own
  memory, which only you read, so a repo you don't trust can't put its instructions in your teammates' sessions.
  A file is sent again only when it changed. The server turns them into facts even where nothing else is
  extracted, so your first sessions already recall something.
- **Commands.** `/memstore:login [<server>] [--org <org>]` signs in, `/memstore:outcome good|bad [why]` rates the session, `/memstore:remember <text>` remembers a note, `/memstore:search <words>` searches the memory, `/memstore:brief <name> [refresh]` says where something stands from its summary, `/memstore:forget <episode id or words>` forgets something you remembered, and `/memstore:status` (`memstore doctor` in a terminal) says whether the memory works here: where it is and whether it's up, who you're calling as and until when, the org, what the memory runs there (extraction, advice learning and its evidence bar), and how the last hook went, with any warning it had.
- **Tools.** The memstore MCP server's tools are available to Claude: by default `start_session`, `get_advice`, `end_session`, `search_memory`, `get_episode`, `remember_memory` and `forget_memory`, plus `get_brief` and `list_briefs` where the schema summarises its subjects, and the rest with `MEMSTORE_MCP_TOOLS=all` on the server.

Words that look like secrets are redacted from everything the hooks send: notes, session traces, transcripts, prompts and outcomes. That covers known key prefixes, JWTs, long random-looking strings, and the value of `password=`, `token:` and the like, including inside JSON and YAML.

## Install

The plugin is in the public [A79-ai/ampup-memory](https://github.com/A79-ai/ampup-memory) repository, whose root is
its marketplace. `memstore-client`, the binary it runs (the sign-in, the hooks and the MCP bridge, a few MB, for
macOS on Apple Silicon and Linux on arm64 or x86_64), is published in that repository's GitHub releases, as the
`client-v*` tags (the server's are `v*`):

```text
/plugin marketplace add A79-ai/ampup-memory
/plugin install memstore@memstore
/memstore:login https://memstore.example.com --org acme
```

With no team server, skip the sign-in and run a local one (below): the plugin uses `http://127.0.0.1:8080` when
nothing else is set.

You don't install the binary yourself. Everything the plugin runs goes through
[`bin/memstore-client`](bin/memstore-client), which runs `memstore-client` from `~/.local/bin` or your `PATH`
(never a full `memstore` instead, which may be older than the plugin). Where it isn't installed, it installs the newest client release,
checking its checksum ([`bin/install.sh`](bin/install.sh)). A session's start installs it in the background and
tells Claude to say once that memory is on from the next session. The other hooks do nothing until then, so a turn
never waits on a download. The MCP server and the commands install it first and then run.

`/memstore:login` signs in through the browser once (`memstore-client join`) as you, with Google or a password, and
the CLI renews the sign-in. The server then says which orgs hold you: one, and you're in it; several, and
`/memstore:login --org <org>` names one; none yet, and `/memstore:login --org <org>` asks to join that org: its admins
are emailed a link that approves you (or they run `memstore requests` and `memstore invite`), and the next
`/memstore:login` gets in. With no server named it signs in to `MEMSTORE_URL`; with neither, it says the memory is the local server, which
needs no sign-in. `MEMSTORE_ORG` names the org for everyone. An organization can set the server and org for everyone, together with
the plugin itself, in Claude Code's managed settings:

```json
{
  "extraKnownMarketplaces": {
    "memstore": { "source": { "source": "github", "repo": "A79-ai/ampup-memory" }, "autoUpdate": true }
  },
  "enabledPlugins": { "memstore@memstore": true },
  "env": { "MEMSTORE_URL": "https://memstore.example.com", "MEMSTORE_ORG": "acme" }
}
```

While nobody is signed in on a machine, a session's start tells Claude to say once that `/memstore:login` turns
memory on. `/memstore:status` (`memstore-client doctor` in a terminal) checks it all.

To install the binary by hand (for Cursor, a terminal, or CI):

```bash
curl -fsSL https://raw.githubusercontent.com/A79-ai/ampup-memory/main/install.sh | MEMSTORE_INSTALL=client sh   # to ~/.local/bin
memstore-client join https://memstore.example.com --org acme
```

`memstore setup <server> --org <org>`, from the full `memstore` install, signs in and sets up the other coding agents
it finds as well. On a laptop with no team server, run one with goal-blind extraction on (Claude Haiku 5.5 on
OpenRouter, the default model). With no context, the plugin uses it:

```bash
MEMSTORE_EXTRACT_GOAL_BLIND=1 OPENROUTER_API_KEY=... memstore serve --local
```

| Variable | Default | What it does |
|---|---|---|
| `MEMSTORE_URL` | the CLI context's server, else `http://127.0.0.1:8080` | Where the memory is, and the server `/memstore:login` signs in to. |
| `MEMSTORE_ORG` | the CLI context's org | The org, with `MEMSTORE_URL`. |
| `MEMSTORE_TOKEN` | the CLI's sign-in to that server | A bearer token to send to `MEMSTORE_URL` instead (an API key, for CI); ignored without `MEMSTORE_URL`. |
| `MEMSTORE_SCOPE` | the team's | `mine` keeps sessions, notes and recall in your own memory, which only you can read. |
| `MEMSTORE_CAPTURE` | on | Set it to `off` to stop sessions being remembered: no trace when they end, and the server keeps nothing you say in them as your memory (from 0.4.6). Advice and recall still work. |
| `MEMSTORE_RECALL` | first prompt | `every` recalls facts on every prompt. Advice comes on the first prompt only. |
| `MEMSTORE_INSTALL_DIR` | `~/.local/bin` | Where the plugin looks for `memstore-client` and installs it. |
| `MEMSTORE_CLIENT_UPDATE` | on | Once a day a session's start checks, in the background, whether a newer `memstore-client` was released and installs it. `off` keeps the one installed; a set `MEMSTORE_VERSION` pins it. |

## In Cursor

The plugin gives Cursor the skills, three commands, the hooks and the MCP server. Like Claude Code, it needs the
`memstore-client` binary on your `PATH` (the `install.sh` above) and a sign-in (`memstore-client join <server> --org <org>`). Cursor's prompt hook can't
add context, so a session gets what the memory knows at its start, but no advice at its first prompt: the
`memstore-advice` skill asks for it instead. Install it from this repository in Cursor's plugin settings
(`.cursor-plugin/marketplace.json` lists it). Use the plugin or `memstore hooks install --harness cursor`, not
both, or every session's hooks run twice.

## In Codex

Codex reads the portable manifest (`plugin.json`, `mcp.json`, `skills/`):

```bash
codex plugin marketplace add A79-ai/ampup-memory      # .agents/plugins/marketplace.json
codex plugin add memstore@memstore
codex mcp login memstore                            # OAuth sign-in, once mcp.json names the team's server
```

`mcp.json` names a local server (`http://127.0.0.1:8080/mcp`, `memstore serve --local`), which needs no sign-in;
a team running its own server changes that URL. The plugin declares no hooks (Codex runs a plugin's hooks only in
its desktop app, after you trust them). For
sessions recorded as runs and advice at the first prompt, also run `memstore hooks install --harness codex`.

## In ChatGPT

ChatGPT reads the same portable manifest. A workspace admin imports the plugin from this repository (**Admin** >
**Plugins**) and chooses who gets it. Each person signs in to the team's server with OAuth when they first use it.
ChatGPT runs no hooks, so the `memstore-advice` skill opens and ends each session. ChatGPT can't reach a server on
your machine, so the workspace's copy of `mcp.json` must name the team's server over HTTPS instead of the local one.

Without hooks, what the user tells the agent outlasts the conversation only if it's written down. Two things make
sure it is. The `memstore-remember` skill and the server's instructions ask the agent to call `remember_memory` in
the turn a preference, correction, decision or procedure is stated. The server also keeps these from the user's
words in the session calls, writing each once (memstore's README, MCP tools, "Kept from what the user
said"). Measured on Claude Code with every hook off, three runs per prompt and arm: Haiku 5.5 kept 18 of 18 and
Sonnet 5.5 15 of 18. The misses were preferences where the agent called no tool at all. A "thanks, that's all"
kept nothing in any run.

## Notes

- **Who can read what.** A session's trace is readable by the person whose session it was alone, never by teammates. Advice learning reads every rewarded session and run in the scope, and only an org's admins start it or turn it on. A remembered session note is yours alone too: it holds what you said, which reaches teammates only once at least three people have said the same (the server's keep and share, in memstore's README). On `serve --local` that is just you.
- **A session is a person's.** With token auth, sessions are kept under your own token. A service key can't keep sessions, so with one the server refuses them and the hook says so on stderr. The session is still remembered if the key may write.
- **Finding the session to rate.** `/memstore:outcome` gets the session id from Claude Code. Run by hand in a terminal (`memstore-client hook outcome good "why"`), it rates the session the hooks opened last. The hooks keep each session's memstore id in an owner-only directory under your temp directory.
- **The transcript format can change.** The session's trace and its note come from Claude Code's transcript file, whose format is internal and can change between releases. Lines the hook can't read are skipped, so a change loses detail rather than breaking the session.
- **Hosts without hooks.** Other MCP clients (Claude Desktop, Cursor and others) can use the same server without the hooks: run `memstore mcp` (signed in as the CLI is) where the host starts a command, or point it at `<server>/mcp`. The [`memstore-advice`](skills/memstore-advice/SKILL.md) skill describes the loop for them: `start_session` first, `get_advice` when a task starts, `end_session` with a condensed transcript and the outcome at the end. Add it as a skill where the host takes skills, or put it in the agent's instructions.
- **Codex, Gemini CLI, Cursor and Copilot.** `memstore hooks install --harness <name>` registers the same hooks. Codex and Gemini get the advice at the first prompt; Cursor and Copilot can't add context at a prompt, so they get no advice. All four end their session with its trace when it ends. Their traces hold what was said, without tool calls, because those transcripts are read for text only. You can rate the last session from a terminal with `memstore hook outcome good|bad [why]`.
