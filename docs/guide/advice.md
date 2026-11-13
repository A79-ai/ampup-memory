# Advice, sessions and procedures

This is the part of AmpUp Memory that improves with use. An agent asks for advice before a task; the memory
answers with what it learned from earlier runs of that kind of task, or with a procedure compiled from your
documents; how the run went teaches the next answer.

All of it needs a model. With no model configured, `get_advice` is not listed over MCP and `POST /advice` answers
503 ("advice is routed by a model: this server has none").

## Sessions

A session is one conversation, start to end.

| Step | MCP | HTTP |
|---|---|---|
| Open, and get advice for the opening context | `start_session {context, harness?}` | `POST /sessions` |
| Ask for advice for a task | `get_advice {session_id, question, conversation_so_far?, moment?}` | `POST /advice` with `session_id` |
| Close, with transcript and outcome | `end_session {session_id, transcript?, outcome?, reward?, reason?}` | `POST /sessions/end` |

- `moment` says where in the task the agent is. `start` (the default) routes the task to its note. `fork`,
  `stuck` and `done` re-read the note the session was already served (steps and pitfalls, pitfalls, or pitfalls
  and verification), with no model call: the person's own note and the team's answer both, when they were served
  together. `next` is sent by the Claude Code hooks when a step finishes.
- Each note is served once per session; asked again, the answer says it was given earlier.
- `get_advice` with no `session_id`, from a person, opens a session and returns its id.
- A session nobody ends is ended after `AMPUP_MEMORY_SESSION_IDLE_SECS` (1800) without a step, with the last
  conversation it was sent as its transcript.
- The opening context, transcript and conversation are the caller's own episodes, readable by them alone, with
  secret-looking words redacted. At most `AMPUP_MEMORY_SESSIONS_PER_DAY` (1000) sessions a person a day.

### Judging

When a session that was served advice ends, a judge model reads its notes and transcript (fenced as data) and says,
per note, whether the agent applied it and whether it helped, and whether the session succeeded, failed or is
unclear. A session's judged outcome is its reward when nobody gave one. Each note keeps a tally (served,
applied, helped, not helped), counting each person's latest judged session only.

### The person's rating

The person whose session it was can rate it: in Claude Code or Codex, `+` or `-` typed alone as a prompt (the hooks
record it and keep it from the agent; see [coding agents](coding-agents.md)), or `/ampup-memory:good|bad [why]`.
The hooks send `POST /runs/reward {run_id: <session id>, reward: 1 or -1, reason?, by: "person"}`. With `by:
"person"` the reward is also the session's rating (`helpful` or `not_helpful`), and:

- learning reads it as the person's response, above the judge's reading and a reward the agent gave itself (only
  what became of the session's PRs and a reviewer's grade rank higher), with their words as the reason;
- the judge still reads the session, but never replaces its reward;
- it counts in each note's tally of outcomes people gave (`checked` in the advice trail).

A session open or ended a moment ago takes the rating at once (it is held with the session's row), and a newer one
replaces it. `by` is taken on the route alone: the `reward_run` MCP tool, whose arguments a model chooses, ignores it.

### Holdout arm

- `AMPUP_MEMORY_ADVICE_HOLDOUT` (0 to 1, default 0): that share of new sessions is routed but served nothing.
  Comparing served against withheld outcomes measures lift.

### Evidence

Every note learning writes is served to everyone it is learned for; there is no trial arm and no gate. A note is
reported ratified once it helped more people than it didn't, counted by each person's latest judged session, up to
the scope's evidence bar: the team's `advice_evidence` setting, else the org's, else `AMPUP_MEMORY_ADVICE_EVIDENCE`
(default 3). The bar is reported only (`list_advice`, `ampup-memory doctor`); it changes nothing that is served.
Runs are not evidence; only judged sessions are.

## Runs and rewards

A run is one task an agent did, recorded by a harness, a CI job or a script.

```bash
curl -s localhost:8080/runs -d '{"task": "deploy billing", "trace": "ran ./deploy.sh billing", "outcome": "deployed"}'
# {"run_id": "…"}
curl -s localhost:8080/runs/reward -d '{"run_id": "…", "reward": 1, "reason": "deployed cleanly"}'
```

- `POST /runs {task, trace, used?, outcome?, advice?}`. `trace` is at most 20,000 characters and is the caller's
  own episode. `advice` is the situation key `get_advice` named, so a run says what it was served (several keys
  separated by `; `).
- `POST /runs/reward {run_id, reward: -1..1, reason?, by?}`. Only the recorder rewards its own run or session.
  `by: "person"` marks it the person's own rating ([above](#the-persons-rating)).
- **Batches.** `POST /runs {"runs": [...]}` takes up to 1,000 runs and 16 MiB, each with an inline `reward` and
  `reason`. Every run is checked before any is written; one bad run refuses the batch. Batch run ids are derived
  from the runs' content, so loading the same runs into a fresh store gives the same ids.
  `POST /runs/reward {"rewards": [...]}` rewards up to 1,000 runs at once.
- **From a file.** `ampup-memory runs load <scope> runs.jsonl --as <user>` records one run a line as one batch.
  With `--local <dir>` (server stopped) it writes a `serve --local` store.
- Limits: `AMPUP_MEMORY_RUNS_PER_DAY` (100) runs and `AMPUP_MEMORY_LOG_PER_DAY` (1000) answer-log entries per person a day.
- Unrewarded runs older than `AMPUP_MEMORY_ADVICE_JUDGE_AFTER_SECS` (1800) are judged by a model (1, -1 or "can't
  tell"); a recorder's own reward later replaces the judgement. `AMPUP_MEMORY_ADVICE_JUDGE=off` turns that off.
  `ampup-memory qa-log <scope>` shows each judgement.

## How notes are learned

1. **Sort.** Rewarded runs and sessions are sorted, 60 tasks a batch, into a map: category, then situation. Only
   the task text is shown to the model.
2. **Link.** Each situation is linked to up to three compiled procedures that serve it.
3. **Write.** For each situation with at least `--min-wrong` (default 2) runs that went wrong, the learning model
   reads up to 40 of its runs (newest wrong and newest right, at least 10 right when it has them) and the linked
   procedures, and writes one note: *When*, *Steps*, *Pitfalls*, *Evidence*. It is a general procedure and never
   names a record's values.
4. **Keep.** Notes are versioned documents: `advice/index`, `advice/map`, `advice/<category>/<situation>`. A note
   is rewritten only after `AMPUP_MEMORY_ADVICE_NOTE_MIN_NEW` (5) new runs or 2 new wrong ones.

Rules that matter:

- **Team notes need people.** A team note is learned only from situations whose runs come from at least
  `advice_learn_people` people (`AMPUP_MEMORY_ADVICE_LEARN_PEOPLE`, default 3, never below 2). One person's traces
  can't make a team note.
- **Personal notes.** A person with at least `AMPUP_MEMORY_ADVICE_MIN_NEW` (10) rewarded runs or sessions of their own
  also gets notes learned from those alone, served to them first, with no evidence bar.
- **On its own.** Where an org's or team's admins set `advice: true`, `serve` learns every `AMPUP_MEMORY_ADVICE_SECS`
  (600) once `AMPUP_MEMORY_ADVICE_MIN_NEW` (10) new rewarded runs are in. `serve --local` learns in its own scope
  without the setting. `AMPUP_MEMORY_ADVICE_LEARN=off` stops the timer; `AMPUP_MEMORY_ADVICE=off` stops learning.
- **By hand.** `ampup-memory advice <scope> [--acl …] [--min-wrong N] [--local DIR]`, or `/admin {"op":
  "learn_advice", "org", "team"?}` on a server.
- **Audience.** Learning reads every rewarded run in the scope, so only an org's admins start it or turn it on,
  and the notes are written for the audience they name (`--acl`, default `*`).

### Trust

A reward is its recorder's word, and the learning model reads reasons and traces as evidence. Anyone who may
record runs in a scope can steer what its notes say. Learn advice in scopes whose writers you trust, and read the
notes (`get_episode {document: "advice/…"}`) before relying on them. A judged reward is a model's opinion of the
trace's end, not the outcome.

### Curation (opt-in)

`advice_curation: true` (or `AMPUP_MEMORY_ADVICE_CURATION=on`) adds three things to each learn:

- **Withhold by reward.** A note is withheld when the runs served it score lower than its situation's other runs,
  with at least `AMPUP_MEMORY_ADVICE_RETIRE_MIN` (8) runs a side, a margin of `AMPUP_MEMORY_ADVICE_RETIRE_MARGIN` (0.2) and
  a one-sided z of at least 1.645. Only rewards somebody gave count, unless `AMPUP_MEMORY_ADVICE_RETIRE_JUDGED=on`.
- **Edit, don't rewrite.** The model returns item edits (add, modify, remove); an edit that would drop more than
  a quarter of the note's words is refused.
- **Merge.** Situations that are the same task under two names are merged; the old names stay as aliases.

## What `get_advice` serves

The routing model reads one line per kind of task the caller may be advised on and names one, or none.

- A situation with a note: **the note alone** (the default). `advice_procedures: true`, or `"procedures": true`
  on a call, serves the note with its linked procedures.
- A situation with no servable note: up to two linked procedures in full.
- A compiled skill no situation links: its procedure.

The MCP answer starts `Situation: <key>`, then `Skill: <key> (version <n>), learned from <n> runs …`.
`POST /advice` answers `{situation, advice, skills}`; each skill says `from: "runs"` or `from: "documents"`.

Options on `POST /advice`: `format` (`notes` or `checklist`; also `AMPUP_MEMORY_ADVICE_FORMAT`), `max_chars` (at
least 500), `procedures`, and `"notes": false` to serve compiled procedures only (an ablation a caller runs
itself).

- **Large libraries.** Above `AMPUP_MEMORY_ADVICE_ROUTE_SHORTLIST` (60) lines, lines are first ranked against the
  question (BM25 and embedding, fused by reciprocal rank) and the router sees the best 60.
- **Prompt caching.** The index goes in the request's system part, marked cacheable where the provider needs a
  mark, so repeated questions reuse it.
- **Lenient reading.** The router's reply is matched by key, by key without its prefix, or by readable name.

### Review checks by file (`POST /advice` with `files`)

`{"files", "title"?, "pr"?, "repo"?, "issues"?, "top"?}` returns review checks for a change: skills whose source
pull requests touched the same files, ranked by shared files (rare files count most), condensed by the routing
model into 2 to 8 bullets. Answers `{checks, skills, issues}`. No session.

### Similar past errors (opt-in)

With `error_leads: true` on an org or team, a question that carries an error (a Sentry short id or link, an
exception and message, or stack frames) also gets up to three leads: similar past errors from the caller's
`sentry:` episodes and the merged `github:` pull requests that name them. Matching is by exception, culprit,
message, shared frames and files, fused with BM25. No model is called. Answers carry `leads`.

### Public capabilities

`get_advice` and `POST /advice` may also suggest a public skill, plugin or MCP server from a small reviewed catalog
(catalog version `2026-11-08`) when a decisions model (`AMPUP_MEMORY_DECIDE`) is configured and is at least 0.9
confident. Send `available_capabilities` to suppress what the agent already has. `public_capabilities: false` per
call, or `AMPUP_MEMORY_PUBLIC_CAPABILITIES=off`, turns them off. They are suggestions, not installs.

## Advice from a repository, from the first session

A new person's memory has no notes: they are learned from rewarded sessions. So that a coding agent is advised in
its first session in a repository, the hooks send what the repository already says about how its work is done, and
the server writes it into notes for the kinds of task coding sessions are routed to.

- **When.** In the background copy of a session's start (never in the hook the session waits on), at most once a
  day per repository and memory, with a lock so sessions starting together send it once. The first time, the
  session is told what was read; later days send it again quietly, and the server keeps only what changed. The
  notes are written again only when a doc changed or five pull requests are new: each write is one call of the
  learning model over the whole material.
- **What is read.** Only files git tracks (so `.gitignore` holds), never code: the agent instructions (`CLAUDE.md`,
  `AGENTS.md`, `GEMINI.md`, `.claude/rules`, `.cursor/rules`), the guides at the top or under `.github/`
  (`CONTRIBUTING`, `README`, `DEVELOPMENT`, `ARCHITECTURE`, `TESTING`, style and release guides, the pull request
  template), up to 15 Markdown files under `docs/`, your own Claude Code memory files for the repository, and the
  last 25 merged pull requests (title, description, changed files, reviews and review comments, bots left out)
  through `gh` where it is signed in, else the default branch's last 60 commit messages and the files they changed.
  At most 30 KB a file and 500 KB in all; links out of the repository aren't followed; secret-looking words are
  redacted.
- **What it becomes.** One call of the learning model writes a note for each of seven kinds of task the material
  says something specific about: implement a requested behavior change, investigate a bug, error or failing test,
  review a pull request, open a pull request, plan how to build a feature or fix, explain how something works in
  the code, run, build or test the project locally. These are the situations advice routing matches most coding
  requests to, so the router and the fit check read them as they read learned notes; narrow compiled procedures
  rarely pass the fit check. Each note has the usual sections, every item tagged with its kind and resting on the
  sources it cites (an item citing none is dropped), within the note budget. They go under a category named for the
  repository (`<repo>-repository/<situation>`).
- **Whose.** The material is your own episodes (`mcp:remember_memory:<you>:memstore:repo:<repo>/<name>`), and the
  notes your own advice (`advice-mine/`), readable by you alone; nothing reaches a teammate. On `serve --local`,
  whose one caller is everyone, they go to the scope's advice. Once runs of one of these kinds of task are learned
  from, learning writes that note from them instead; until then learning keeps the seeded notes.
- **Without a model** (none configured, or `AMPUP_MEMORY_ADVICE=off`), nothing is read or kept: the session is told
  why once, and the next day's start asks again.
- `AMPUP_MEMORY_REPO_ADVICE=off` (or `AMPUP_MEMORY_CAPTURE=off`) on the client sends nothing. The route is
  `POST /advice/seed` ([http-api.md](http-api.md#the-learning-loop)): at most 80 sources and 600 KB a call, and 20
  calls that keep something new per person a day on one server.

## Procedures: AIP skills and step graphs

A scope's chosen documents are compiled into a skill library as they come in. A skill is what an agent should do in
one situation the documents describe:

- the situation (*When*);
- numbered steps with the exact tools, values and checks the sources give;
- an *If* line per edge case;
- the sources it rests on.

Each new or changed skill is also compiled into an **AIP step graph** (purpose, triggers, when not to use it, and
ordered kebab-named steps with named inputs and outputs: the portable prose subset of AIP v0.3). Both are versioned
documents: `skills/<slug>`, `aip/<slug>`, `skills/index`.

```bash
ampup-memory org settings acme --compile-sources kb:,github:acme/        # what the org compiles from
AMPUP_MEMORY_COMPILE_SOURCES=kb: ampup-memory serve --local                   # a laptop server's own scope
ampup-memory compile team:acme/support --sources kb: --branch arm1        # once, on a branch
ampup-memory compile org --sources kb: --local ./store                    # a laptop store (server stopped)
```

- **Chosen, never on by default.** A scope compiles from its `compile_sources` (source prefixes, or `*`), else
  `AMPUP_MEMORY_COMPILE_SOURCES`. With none, nothing compiles. Versioned documents are never compiled.
- **Batches.** Sources go to the model 12 at a time (at most 60,000 characters), oldest first; each round sees the
  library the last one left. Tune with `AMPUP_MEMORY_COMPILE_BATCH`, `AMPUP_MEMORY_COMPILE_BATCH_CHARS`,
  `AMPUP_MEMORY_COMPILE_MAX_TOKENS` and `AMPUP_MEMORY_COMPILE_PARALLEL`.
- **Grounded.** Sources go in under handles (`SRC1`, …); a skill that cites none is dropped, and a graph that
  fails validation isn't written.
- **Audiences.** Sources with different ACLs never share a batch. A skill is readable by exactly its sources' ACL.
- `compile_kind: errors` tells the compiler its sources are error reports, so each skill's *When* names the
  exception and message.
- `AMPUP_MEMORY_COMPILE=off` turns compiling off for the server; `set_settings {"compile": false}` for one org or team.

## Models

| Work | Default | Override |
|---|---|---|
| Learning advice (sort, link, write) | Claude Opus 5.5 | `AMPUP_MEMORY_ADVICE_MODEL` |
| Compiling skills | Claude Opus 5.5 | `AMPUP_MEMORY_COMPILE_MODEL` |
| AIP graphs | the compile model | `AMPUP_MEMORY_AIP_MODEL` |
| Routing `get_advice` | Claude Haiku 5.5 | `AMPUP_MEMORY_ADVICE_ROUTE_MODEL` |
| Judging sessions and runs | Claude Haiku 5.5 | `AMPUP_MEMORY_JUDGE_MODEL` |

The learning models run rarely; what they write is read on every later task. Per-call work runs on the small model.
Backends and credentials are in [configuration.md](configuration.md#models).

## Model-call cache

Every model call can be answered from an earlier identical request (backend, model and request body, keyed by
SHA-256). Builds (`compile`, `advice`, `extract`, `aggregate`, `serve --local`) keep replies in
`~/.cache/memstore/model-calls` by default, so rebuilding the same memory pays nothing for calls already made.
Servers keep none unless `AMPUP_MEMORY_MODEL_CACHE` names a location (a directory, `gs://`, `s3://`, `file://`, or
`off`). The cache keeps replies after their data is forgotten: a deployment that must erase data leaves it off or
clears it too.
