---
description: The facts the team memory (AmpUp Memory) holds about something, found by meaning; with nothing named, the newest
argument-hint: "[what about]"
allowed-tools: Bash("${CLAUDE_PLUGIN_ROOT}/bin/ampup-memory-client" facts:*)
---
```!
"${CLAUDE_PLUGIN_ROOT}/bin/ampup-memory-client" facts $ARGUMENTS 2>&1 || true
```

Answer from the facts above, as they stand. With a subject named, they are ranked by meaning: lead with what bears on it most, newest first where they disagree, and say which episode each comes from (the `[e1 t3]` pointers; the Sources list gives each episode's id, which `get_episode` reads). With nothing named, they are the newest facts: summarise them in a few lines, grouped by what they are about. Don't add facts of your own. If it gives a console link, end with it. If nothing matches, say so plainly.
