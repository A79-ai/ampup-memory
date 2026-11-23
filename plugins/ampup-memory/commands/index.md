---
description: Index your last N days of Claude Code sessions into the team memory (AmpUp Memory), and see what your agents paid for twice
argument-hint: "[days]"
allowed-tools: Bash("${CLAUDE_PLUGIN_ROOT}/bin/ampup-memory-client" index:*), AskUserQuestion
---
Index this machine's recent Claude Code sessions into the team memory and show the Memory Replay report.

1. How far back: if `$ARGUMENTS` names a number of days (`14`, `30d`, `2 weeks`), use it. Otherwise ask with AskUserQuestion, "How far back should AmpUp Memory index your Claude Code sessions?", with the options 7 days, 14 days (recommended) and 30 days. Use what they pick, or the number of days they type.

2. Run, with N the days, and a timeout of 10 minutes:

   ```
   "${CLAUDE_PLUGIN_ROOT}/bin/ampup-memory-client" index --days N
   ```

   It reads the sessions that started in those days where Claude Code keeps them, sends the ones the memory doesn't have yet to it (as the plugin's hooks would have at their end), has Claude write notes on the biggest repeats through the person's own Claude Code sign-in (`claude -p`), writes the report and opens it in the browser.

3. Say what it printed in a few lines: how many sessions it read and in which repositories, how many it sent to the memory and how many the memory had already, what was paid for twice and the biggest kinds, and the report's path (the browser opened it).

   Then what the memory holds, from its "What the memory holds" lines, one line a connector: covered, missing early days (it starts inside the window: the days before need a backfill) or quiet (nothing in the window: it may have stopped). For each that isn't covered, say how it is filled: a connector the team's server reads (Slack, a CRM, call recordings) backfills as far as it was set up to when connected, so its admins set it again or run its backfill; a source this machine's server reads (`ampup-memory sources list` shows them) goes further back with `ampup-memory sources add <kind> <name> --since <date>`. Never guess which connectors are connected beyond what it printed: one with nothing in the memory isn't listed.

   Say any failure plainly, with its fix:
   - the notes weren't written: sign in to Claude Code with `claude`, or run `claude setup-token` in a terminal and set `CLAUDE_CODE_OAUTH_TOKEN` to the token it prints, then run `/ampup-memory:index` again;
   - nobody is signed in to the memory: `/ampup-memory:login`;
   - `index` is an unknown command: the client is older than the plugin; `"${CLAUDE_PLUGIN_ROOT}/bin/ampup-memory-client" update` installs the newest.
