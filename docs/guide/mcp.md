# MCP

`ampup-memory serve` speaks MCP over streamable HTTP at `/mcp` (POST only; the server is stateless). `ampup-memory mcp`
and `ampup-memory-client mcp` bridge the same endpoint over stdio, signed in as the CLI is, for hosts that start a
command.

```bash
ampup-memory serve --local
claude mcp add --transport http ampup-memory http://localhost:8080/mcp
```

For a server with sign-in, select the org and team with headers (`x-memstore-org`, `x-memstore-team`) or, for
clients that can't set headers, on the URL: `https://memory.example.com/mcp?org=acme&team=sales`.

## Tools offered by default

The default listing is the loop of a conversation:

| Tool | What it does |
|---|---|
| `start_session {context, harness?}` | Opens a session. Answers with a `session_id`, the caller's own notes that bear on the opening context (`remembered_for_you`), what the team shares that does (`team_shared`), within 1,200 characters, and any advice for it. With no context, no notes: the session's first `get_advice` hands the ones that bear on its task. Listed where the memory is written. |
| `get_advice {question, session_id?, conversation_so_far?, moment?}` | The skill for this kind of task. Starts `Situation: <key>`. Listed wherever the server has a routing model. Read-only. |
| `end_session {session_id, transcript?, outcome?, reward?, reason?}` | Closes the session with a transcript of at most 300 words and the outcome. |
| `search_memory {query, k?, start_date?, end_date?, cursor?}` | Facts and source text about a question, as compact cited text. |
| `get_episode {episode_id \| document, offset?, limit?, around?, window?, whole?}` | The source text a fact cites. Episodes over 12 KB come back in pages of their units (turns, messages, paragraphs). |
| `remember_memory {source, body, for?}` | Keeps a note. `for: "me"` (default) is the caller's alone; `for: "team"` is everyone who reads the scope. |
| `forget_memory {episode_id \| matching}` | Forgets one of the caller's own notes. `matching` lists candidates and forgets nothing until confirmed. |
| `get_brief`, `list_briefs` | Where a subject stands, from its summary. Listed only where the schema has `extract.aggregates`. |

A caller is offered only the tools it may call: an ingest key gets `remember_memory` alone, a reader key no
`remember_memory`, a branch or snapshot read neither write tool. Every tool states its MCP annotations
(`readOnlyHint`; writes say whether they are destructive).

`search_memory` also takes `as_of`, `format: "json"`, `predicates`, `episode_type`, `tags`, `order` and
`max_chars` by name; `AMPUP_MEMORY_MCP_TOOLS=all` lists them.

## Other tools

Not listed by default, but callable by name: `record_run`, `reward_run`, `query_memory`, `get_entity`,
`ask_memory`, `skills`, `log_answer`, `feedback_memory`, `close_memory`. `AMPUP_MEMORY_MCP_TOOLS` changes the listing,
never the rights:

- `all`: every tool the caller may call.
- A comma-separated list (`get_advice,query_memory`): those alone.

## Instructions

The `initialize` instructions lead with the memory profile of the org (where one is set) and the session call
order: `start_session` first, `get_advice` when a task starts, `end_session` at the end, `remember_memory` in the
turn a user states a preference, correction, decision or procedure.

## What the server keeps from the user's words

Hosts without hooks (Claude Desktop, ChatGPT, an IDE) don't always call `remember_memory`. So each message the
person types, as it reaches the server, is read once by one small model call, with the end of the assistant's reply
before it: `start_session`'s context, a hook's question, the conversation a hook sends every few minutes, and the
transcript at the session's end. The preferences they show there, how they want things done in every session from
now on, are kept as the person's own notes. Instructions for the task at hand, facts and decisions aren't: what a
session's work taught (a correction, a step that went wrong) is learned as the person's own advice from its graded
requests, and the `[preference]` items of that advice are handed at a session's start with their notes. An agent's
own `get_advice` question and its account of a session are its words, not the person's, and keep nothing; neither
does a recorder key's session. Only a caller with a `user:` entry of their own has anything kept, so
`serve --local` and service keys keep nothing this way. `AMPUP_MEMORY_KEEP=off` turns it off. Sessions themselves aren't read for
facts, neither the record of one that ended nor its conversation so far, unless `AMPUP_MEMORY_EXTRACT_SESSIONS=on`.

**Filed in a map.** Each kept note is filed under a category and a topic, picked from the person's own map so far
(a new one only when none fits), as `keep:<category>:<topic>` after their signature, so a person's notes read as
folders. A note their kept notes already say isn't kept again. Callers can't write under `keep` themselves.

**Shared once three people said it.** A personal note stays the author's until at least three distinct people
have each said the same thing. Then one sentence, built only from words all their notes share, is written as a
note everyone in the scope reads, naming no one. The authors are counted by the server, not the model.

**Profiles (opt-in).** With `profile: true` on an org or team (or `AMPUP_MEMORY_PROFILE=1`), each person's notes are
folded into one deduplicated profile (at most 40 lines, 8,000 characters) that `start_session` hands them with
their newest 5 notes. Forgetting a note repairs the profile at once.

## Hook-only arguments

Arguments that keep or record what a `get_advice` call carries (`max_chars`, `event`, `decide`, `remembered`,
`keep`) are taken only on `/mcp/hook`, the address the hook client calls. They are dropped from calls on `/mcp`, so
a model can't add them to a call its host lets through unapproved.

## Pinned and branch reads

`?at=pin_…` or `?at=snap_…` on the MCP URL pins every tool's reads to a pin or workspace snapshot;
`?branch=<name>` reads a branch. Writes always go to main.
