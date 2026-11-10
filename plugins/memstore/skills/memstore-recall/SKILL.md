---
name: memstore-recall
description: Read the team memory (memstore) before guessing, and answer from it with sources. Use when the task depends on history you weren't given - what the team decided, said, promised or knows about a customer, project, person, incident or piece of code, where something stands, or a preference the user set before - and the memstore tools search_memory and get_episode are available.
---

# Recall from memstore, with sources

memstore holds the team's memory as facts extracted from its conversations, calls, documents, systems and past
agent sessions. Each fact cites the episode (a Slack thread, a call, an email, a doc, a session) it comes from.
Read it before you guess or ask the user something the team already settled, and show where each answer comes
from, so the user can check it.

## Where something stands: its summary first

Where the memory summarises a kind of subject (a customer, a project, an account, an incident; the team's schema
decides which), `get_brief` returns where one stands in one read: how it scores on what the team tracks, with the
evidence, the gaps, the open and closed items, and its current facts. If `get_brief` is offered and the question
is where something stands, read its brief first:

- The subject's id is `type:name`, such as `project:checkout_v2` or `customer:acme`. `list_briefs` finds it:
  pass the name as `query` (it matches on letters and digits, so "acme corp" finds `customer:acme_corp`), or
  nothing for the subjects summarised most recently.
- The brief's `summarised_at` says when the summary was made. Say it when it's more than a day old or the
  question is about something recent. `refresh: true` makes it again from what came in since (a model call,
  at most once every 5 minutes, nothing when nothing is new): pass it only when the user wants it current.
- If it says `summarised: false` or `partial`, say so and search for the rest.

## Search

1. Call `search_memory` with the words the answer would use: names, products, the thing decided. Not the
   whole question. `type` narrows to one kind of subject and `predicates` to kinds of fact, when you know
   them.
2. Each result names a subject and its facts, newest first, each with a pointer such as `[e1 t42]`: episode
   `e1` of the answer's Sources list, turn 42. Keep those pointers in your answer.
3. When the first page doesn't answer it, search again with other words (the name someone else uses, an old
   name) or pass the answer's `cursor` for the next page. A question with several parts takes several searches.

## Read the source when it matters

When a fact is the crux of the answer, or it's ambiguous, read it in context with `get_episode`: the episode
id from Sources, and `around` set to the unit the pointer names (the `42` of `t42`, the `3` of `p3`). A long
episode comes back in pages; `next_offset` reads on.

## Answer

- State what the memory says, with the pointer or episode after each claim, and when it was said where that
  matters. Where facts disagree, say so, newest first.
- Apply what the user told the memory before (a preference, a correction) without making them repeat it.
- A fact `written_by: posted` came from an agent, a script or an import rather than a conversation. Say so
  when it decides the answer.
- If the answer says nothing in memory mentions what you searched for, or only gives results nearest in
  meaning, say plainly that the memory has no answer. Never fill the gap with a guess.
- You only see what you may read. Don't speculate about what others may hold.

If a memstore call fails, say so in a line and answer from what you have.
