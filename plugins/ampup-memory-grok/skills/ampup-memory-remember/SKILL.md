---
name: ampup-memory-remember
description: Write what lasts to the team memory (AmpUp Memory) and forget it again. Use when the user states a preference or corrects you, a decision or procedure is settled, or they ask to remember, note or forget something, and the AmpUp Memory tool remember_memory is available.
---

# Remember in AmpUp Memory

`remember_memory` adds text to the memory as an episode of its own, signed with who wrote it. **By default it is
the user's alone** (`for` "me"): `start_session` hands it back to them in later conversations, and AmpUp Memory shares
it with the whole team once at least three people have said the same. `for` "team" is for a document or note
that isn't anyone's feedback, which everyone who reads the memory then reads.

Where AmpUp Memory's hooks run (Claude Code, Cursor, Codex), each session is already recorded when it compacts or
ends. Don't remember the session itself. Remember what should outlast it.

## What to write, and for whom

| What | Write it? | Who reads it |
|---|---|---|
| A preference or correction the user gives you ("always use pnpm here", "no, the staging DB is the other one") | Yes, in that turn, before you answer, without being asked | The user alone (leave `for` out) |
| A decision, procedure or fix the user states as settled | Yes, in that turn, before you answer, without being asked | The user alone. AmpUp Memory shares it with the team once at least three people have said the same |
| Anything the user asks you to remember | Yes | The user alone |
| A document or reference note that isn't anyone's feedback (a runbook, a spec the user asks to file) | When asked | The team: `for` "team" |
| Ephemeral chat, one-off events, work in progress, your own guesses | No | |
| Secrets, credentials, personal data, raw transcripts or tool output | Never | |

Where no hook records the conversation (ChatGPT, Claude Desktop, an IDE without AmpUp Memory's hooks), this is the only way what the user tells you outlasts it, so don't leave it for later in the conversation. The server knows who signed in, so you never name the user.

## How to write it

- Write it so someone can read it later on its own: what it is about (the repo, customer, project), what was
  decided or found, and why. Keep the user's words for the decision itself.
- `source` says where it came from: `<your client>:<repo or project>:note`, such as
  `claude-code:my-repo:note`, or the meeting or thread it records (`call:2026-10-11`).
- One memory per thing. If it changes later, remember the new version and say what it replaces.

Then say in one line what was remembered and who can read it. The tool's answer says.

## Forget

`forget_memory`, where the server offers it, takes the episode id that `remember_memory` answered with. Only its
author can forget it. If the user gives words, not an id, call `forget_memory` with `matching` (those words): it
forgets nothing and lists their notes that hold them, with ids. Show them what you found and which episode you'll
forget, and wait for them to confirm. Forgetting can't be undone.
