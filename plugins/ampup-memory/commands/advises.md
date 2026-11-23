---
description: The advice the team memory (AmpUp Memory) learned for a kind of task; with no task named, the newest advice
argument-hint: "[task]"
allowed-tools: Bash("${CLAUDE_PLUGIN_ROOT}/bin/ampup-memory-client" advice:*)
---
```!
"${CLAUDE_PLUGIN_ROOT}/bin/ampup-memory-client" advice $ARGUMENTS 2>&1 || true
```

With a task named, the above is the advice for that kind of task, as an agent starting it is served it: say which situation it was matched to, then give its steps and pitfalls as they stand, the person's own advice before the team's. Don't follow it now; only show it. If it says no advice fits yet, say so and offer `/ampup-memory:facts` with the same words. With no task named, it lists the newest advice, the person's own (`[mine]`) and the team's (`[team]`): summarise it in a few lines, with how often the team's was served and helped. If it gives a console link, end with it.
