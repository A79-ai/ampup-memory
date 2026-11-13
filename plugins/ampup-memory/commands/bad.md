---
description: Say this session's task went badly, so the team memory (AmpUp Memory) learns from it (a bare - does the same)
argument-hint: "[why]"
allowed-tools: Bash("${CLAUDE_PLUGIN_ROOT}/bin/ampup-memory-client" hook outcome:*)
disable-model-invocation: true
---
```!
"${CLAUDE_PLUGIN_ROOT}/bin/ampup-memory-client" hook outcome --session=${CLAUDE_SESSION_ID} 2>&1 <<'AMPUP_MEMORY_OUTCOME'
bad $ARGUMENTS
AMPUP_MEMORY_OUTCOME
```

Report AmpUp Memory's line above in one line, as it stands. Don't rate the run yourself or change the verdict.
