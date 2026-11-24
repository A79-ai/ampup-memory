---
description: Index your last 30 days of Claude Code sessions into the team memory (AmpUp Memory), and see what your agents paid for twice
argument-hint: "[days, default 30]"
allowed-tools: Bash("${CLAUDE_PLUGIN_ROOT}/bin/ampup-memory-client" index:*), AskUserQuestion
---
Index this machine's recent Claude Code sessions into the team memory and show the Memory Replay report.

1. How far back: if `$ARGUMENTS` names a number of days (`14`, `30d`, `2 weeks`), use it; otherwise 30 (Claude Code keeps 30 days of sessions by default, so that is all there is). Never ask.

2. What it will take. Run, with N the days:

   ```
   "${CLAUDE_PLUGIN_ROOT}/bin/ampup-memory-client" index --days N --estimate
   ```

   It sends nothing. Then ask with AskUserQuestion, "Who should write the Memory Replay notes?", saying in the question what it printed: how many sessions it will send (and how many it skips as too short), and the tokens, about. The options:
   - **AmpUp's key (recommended)**: written by Claude on the memory's server, on AmpUp's own model account (its key stays on the server); nothing to set up, and the repeats it is shown are only the redacted samples the report already has.
   - **My own Claude Code**: written through this machine's Claude Code sign-in (or a token from `claude setup-token` in `CLAUDE_CODE_OAUTH_TOKEN`), on the person's own plan.

3. Run, with a timeout of 10 minutes, `--notes-with memory` for AmpUp's key or `--notes-with claude` for their own:

   ```
   "${CLAUDE_PLUGIN_ROOT}/bin/ampup-memory-client" index --days N --notes-with memory
   ```

   It reads the sessions that started in those days where Claude Code keeps them, sends the ones the memory doesn't have yet to it (as the plugin's hooks would have at their end; sessions too short to learn from are skipped, and there is no daily most on them), has Claude write notes on the biggest repeats, writes the report and opens it in the browser.

4. Say what it printed in a few lines: how many sessions it read and in which repositories, how many it sent to the memory, how many the memory had already and how many were too short to keep, the tokens the notes took, what was paid for twice and the biggest kinds, and the report's path (the browser opened it).

   Then what the memory holds, from its "What the memory holds" lines, one line a connector: covered, missing early days (it starts inside the window: the days before need a backfill) or quiet (nothing in the window: it may have stopped). For each that isn't covered, say how it is filled: a connector the team's server reads (Slack, a CRM, call recordings) backfills as far as it was set up to when connected, so its admins set it again or run its backfill; a source this machine's server reads (`ampup-memory sources list` shows them) goes further back with `ampup-memory sources add <kind> <name> --since <date>`. Never guess which connectors are connected beyond what it printed: one with nothing in the memory isn't listed.

   Say any failure plainly, with its fix:
   - the notes weren't written through their own Claude Code: sign in to Claude Code with `claude`, or run `claude setup-token` in a terminal and set `CLAUDE_CODE_OAUTH_TOKEN` to the token it prints, or pick AmpUp's key, then run `/ampup-memory:index` again;
   - the notes weren't written on AmpUp's key: offer to run it with `--notes-with claude` instead;
   - nobody is signed in to the memory: `/ampup-memory:login`;
   - `index` is an unknown command, or `--estimate` or `--notes-with` an unexpected argument: the client is older than the plugin; `"${CLAUDE_PLUGIN_ROOT}/bin/ampup-memory-client" update` installs the newest.
