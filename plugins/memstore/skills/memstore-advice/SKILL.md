---
name: memstore-advice
description: Run each conversation as a memstore session so the team memory can advise on the task and learn whether its advice helped. Use whenever the memstore tools start_session, get_advice or end_session are available, at the start of a task-oriented conversation, when you need help, when a step finishes and you move to the next, and when the conversation ends.
---

# memstore advice, over a session

memstore learns advice from how past sessions went: for a kind of task, the steps that got the right outcome
and the pitfalls of the ones that didn't. A session is how it learns whether its advice works. You open one at
the start, ask for advice as tasks come up, and end it with what happened. The server then judges, for each
piece of advice it gave, whether you applied it and whether it helped.

## Is a session already open?

Look in your context for a line starting `memstore session:`. If it is there, the memstore hooks opened the
session and will end it. Use the `session_id` it gives, skip step 1, and leave step 4 to the hooks.

## The calls

1. **Start.** Before your first answer, call `start_session` with `context`: what the user asked, with
   anything that came with it (policy, constraints, the record they named). Pass `harness` if you know your
   client (`claude-ai`, `chatgpt`, `codex`, `cursor`, `copilot`). Keep the `session_id` it returns for the whole conversation.
2. **Follow the advice.** If it returns advice, follow its steps and avoid its pitfalls, unless the data or the
   user says otherwise. When you deviate, say why in your answer. If it returns none, carry on as usual. Its
   `Skill:` line names the skill it served: learned from past runs (how many, right and wrong) or compiled from
   the org's documents. Its evidence line says how often it has helped before. Advice with no evidence yet is a
   candidate, so check its steps against the data before relying on them. The memstore `skills` tool, where it
   is offered, lists every skill the memory has learned.
3. **Ask whenever it would help, and at each step boundary.** Call `get_advice` with the `session_id`, the
   task as `question`, a few lines of `conversation_so_far`, and the `moment`:
   - `start` (the default): before substantive work. Orient first (find the files, read what's there), then
     ask before you write, edit or answer. Ask again with `start` when the user moves to a different kind of
     task.
   - `next`: a step is finished (the file is written, the test passes, the question answered) and you are moving
     to the next. Pass the next step as `question`. A finished step is the cheapest time to learn what the
     steps ahead usually trip on. If the next step is a different kind of task, it routes it afresh.
   - `fork`: choosing between approaches, or about to change something shared or hard to undo.
   - `stuck`: you need help: the same error came back, the approach isn't converging, or you are unsure.
   - `done`: about to declare the task finished. Save the result first (write the file, commit).

   A later moment re-reads the advice the session was given with no model call, so asking often is cheap. If it has nothing, carry on.
4. **End.** When the conversation is over (the user is done, or the task is finished or abandoned), call
   `end_session` with the `session_id` and:
   - `transcript`: a condensed account of what was asked and what you did, including the tool calls that
     mattered, which advice steps you followed or skipped, and why. Leave out secrets and credentials.
   - `outcome`: how it turned out, in a sentence.
   - `reward` (-1 to 1) and `reason`: only when the user confirmed the result was right or wrong. Never
     guess one.

If you never end the session, the server ends it after a while from the last `conversation_so_far` you sent.
An explicit end gives the judge a better transcript.

## Notes

- Don't tell the user about the session mechanics unless they ask. Just do the work.
- The same `session_id` is good for the whole conversation. Never reuse one across conversations.
- If a session call fails, carry on without it. memstore is never a reason to stop helping.
