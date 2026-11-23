---
description: What the team memory (AmpUp Memory) holds, and what it learned and served this week
allowed-tools: Bash("${CLAUDE_PLUGIN_ROOT}/bin/ampup-memory-client" stats:*)
---
!`"${CLAUDE_PLUGIN_ROOT}/bin/ampup-memory-client" stats 2>&1 || true`

Report the above in a few lines, keeping its numbers as they stand: what the memory holds, what came in over the last seven days and from where, the lessons learned and the advice served this week (and how many wait for ratifying). If it gives a console link, end with it. If it says something is still being counted, say to run `/ampup-memory:stats` again shortly. If AmpUp Memory isn't reachable or refused the caller, say so and suggest `/ampup-memory:status`.
