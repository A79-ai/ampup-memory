---
description: Forget something you remembered in the team memory (memstore)
argument-hint: "<episode id, or words to find it by>"
---
Forget a memory with the memstore `forget_memory` tool. It needs the episode id that `remember_memory` answered with. If the text below is not an episode id (64 hex characters), call `forget_memory` with `matching` set to it first (it forgets nothing and lists my notes that hold those words, with their ids), show me what you found and which episode you will forget, and wait for me to confirm before calling `forget_memory`. Only its author may forget a memory; if the tool says no such episode was remembered by me, say so plainly.

$ARGUMENTS
