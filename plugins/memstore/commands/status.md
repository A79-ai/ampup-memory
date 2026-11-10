---
description: Whether the team memory works here - where it is, who you are in which org, what it runs, and how the last hook went
allowed-tools: Bash("${CLAUDE_PLUGIN_ROOT}/bin/memstore-client" hook status:*)
---
!`"${CLAUDE_PLUGIN_ROOT}/bin/memstore-client" hook status 2>&1 || true`

Report the above in a few lines: where the memory is and whether it's up, who you're calling as and in which org, whether extraction and advice learning are on, and how the last hook went (say any warning it had). If memstore isn't reachable, say how to fix it: `memstore join <server> --org <org>` for the team's server, or `memstore serve --local` on this machine. If the sign-in expired or was refused, `memstore join` again.
