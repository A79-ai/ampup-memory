---
description: Say how this session's task turned out, so the team memory (memstore) learns from it
argument-hint: "good|bad [why]"
allowed-tools: Bash("${CLAUDE_PLUGIN_ROOT}/bin/memstore-client" hook outcome:*)
disable-model-invocation: true
---
```!
"${CLAUDE_PLUGIN_ROOT}/bin/memstore-client" hook outcome --session=${CLAUDE_SESSION_ID} 2>&1 <<'MEMSTORE_OUTCOME'
$ARGUMENTS
MEMSTORE_OUTCOME
```

Report memstore's line above in one line, as it stands. Don't rate the run yourself or change the verdict.
