---
description: Where something stands (a customer, project, deal), from the team memory's summary of it (memstore)
argument-hint: "<name> [refresh]"
---
Tell me where this stands from memstore, following the `memstore-recall` skill: find its summary with `list_briefs` (the name as `query`), read it with `get_brief`, and lead with where it stands, its gaps and its open items, citing the evidence. Say when it was summarised. Pass `refresh: true` only if the text below says refresh. If nothing like it is summarised, search the memory for it instead.

$ARGUMENTS
