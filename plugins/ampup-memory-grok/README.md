# AmpUp Memory for Grok Bot

AmpUp Memory for Grok Bot is the memory's MCP server plus skills that tell the Bot how to use it. There are no hooks.

## Why no hooks

Grok Bot runs on cloud agents. Its model calls happen on the provider's servers, not on the Bot's computer:

- The only Cursor-format hooks that fire there are around its shell commands: `preToolUse`, `postToolUse`,
  `beforeShellExecution` and `afterShellExecution`. They see no prompt and no answer.
- The conversation is never written to the Bot's computer, so no hook could read a transcript either.

The session therefore comes from the agent. The skills have it:

- open a session when a task starts;
- ask for advice as the task goes;
- remember what the user settles, in the turn they say it;
- close the session with a transcript when the task is done.

| Skill | What it does |
|---|---|
| `ampup-memory-session` | Runs each task as an AmpUp Memory session: `start_session` (harness `grok`), `get_advice` at each step, `end_session` with a transcript when the task is finished. A Bot is one long conversation, so a session is one task, not the whole Bot. |
| `ampup-memory-recall` | Reads the memory before guessing, and answers with sources (`search_memory`, `get_episode`, `get_brief`). |
| `ampup-memory-remember` | Writes preferences, corrections and decisions in the turn they are said (`remember_memory`), and forgets them again. |

The last two are the [AmpUp Memory plugin](../ampup-memory)'s own skills.

## Install

1. **Connect the MCP server.**
   - In Grok Bot, open **Connect apps** and choose **Add a custom connector**.
   - Set **Type** to **Remote HTTPS** and **Server URL** to `https://memory-api.ampup.ai/mcp`. For your own server,
     use `https://<server>/mcp`.
   - Save it, then sign in when it asks. The sign-in tokens stay with the provider's connector service; the Bot
     calls the tools without seeing them.
2. **Save the skill.** Send a Bot this line. It reads [the skill](skills/ampup-memory-session/SKILL.md) from this
   repository and saves it. Private skills are one library shared by all your Bots.

   ```text
   Read https://raw.githubusercontent.com/A79-ai/ampup-memory/main/plugins/ampup-memory-grok/skills/ampup-memory-session/SKILL.md and save it as a private skill called ampup-memory-session, with its description line as the description.
   ```

   If the Bot can't reach GitHub, paste the skill's text instead and ask it to save that.
3. **Turn it on.** In **Connect apps**, turn the skill on for each Bot under **Private skills**.
4. **Check it.** Ask a Bot something, then ask it which AmpUp Memory session it used. The session shows in the
   memory's console with harness `grok`.

## What it can't do

- **Sessions only exist if the Bot opens them.** A Bot that skips the skill leaves no session. The server's own
  instructions ask the same of every MCP client, so the skill is a second nudge, not a guarantee.
- **The transcript is the Bot's summary,** at most 300 words, not the full conversation.
- **A session the Bot never closes** ends on its own after a while, with only the last `conversation_so_far` it sent.
- **On an Enterprise plan, inference hooks would remove the need for the skill.** They are a webhook that receives
  every model call with the whole conversation. AmpUp Memory doesn't take them yet.
