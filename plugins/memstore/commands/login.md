---
description: Sign in to the team memory (memstore) on this machine, through the browser
argument-hint: "[<server>] [--org <org>]"
allowed-tools: Bash("${CLAUDE_PLUGIN_ROOT}/bin/memstore-client" login:*)
disable-model-invocation: true
---
!`"${CLAUDE_PLUGIN_ROOT}/bin/memstore-client" login $ARGUMENTS 2>&1 || true`

Report memstore's lines above in a line or two. If it signed in, say who as and in which org, and that the memory is on from the next session. If it says the person is in no org yet, or in several, say so and that `/memstore:login --org <org>` names the one (in none yet, that asks to join it, and its admins are emailed to approve). If it says the person asked to join the org, say that an admin of the org has been emailed to approve them, and to run `/memstore:login` again once approved. If it says no server was named and the memory is a local server, say that no sign-in is needed for it (`memstore serve --local` must be running), and that a team's server is `/memstore:login <server> --org <org>`, or an admin sets `MEMSTORE_URL` and `MEMSTORE_ORG` for everyone.
