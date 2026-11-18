---
description: Sign in to the team memory (AmpUp Memory) on this machine, through the browser
argument-hint: "[<server>] [--org <org>] [--create-org]"
allowed-tools: Bash("${CLAUDE_PLUGIN_ROOT}/bin/ampup-memory-client" login:*)
disable-model-invocation: true
---
!`"${CLAUDE_PLUGIN_ROOT}/bin/ampup-memory-client" login $ARGUMENTS 2>&1 || true`

Report AmpUp Memory's lines above in a line or two. If it signed in, say who as and in which org, and that the memory is on from the next session. If it says the person is in no org yet, or in several, say so and that `/ampup-memory:login --org <org>` names the one (in none yet, that asks to join it, and its admins are emailed to approve). If it says the person asked to join the org, say that an admin of the org has been emailed to approve them, and to run `/ampup-memory:login` again once approved. If it made the person's own org, say its name, that it is theirs (they are its admin) on a free trial until the date it gave, and that the memory is on from the next session. If it says they could make their own org (`--create-org`), offer it: `/ampup-memory:login --create-org` makes it. If it says they couldn't make one, give its reason in plain words: an org of their own needs a Google or GitHub sign-in whose email the provider verified, so a password account can't; then run `/ampup-memory:login` again and choose Google or GitHub. With no server named it signs in to `AMPUP_MEMORY_URL`, else the plugin's own server. If it says no server was named and the memory is a local server, say that no sign-in is needed for it (`ampup-memory serve --local` must be running). Another team's server is `/ampup-memory:login <server> --org <org>`, or an admin sets `AMPUP_MEMORY_URL` and `AMPUP_MEMORY_ORG` for everyone.
