# Benchmarks

How AmpUp ran every benchmark it publishes for AmpUp Memory (the website calls the product Enterprise Brain), and
the results it claims. Each section below covers one benchmark: what it is, how it was set up, how it was scored,
and the results as published.

| Benchmark | Measures | Baseline | With AmpUp Memory | Difference [95% CI] |
|---|---|---|---|---|
| [ORCA-bench](#orca-bench) | Incident root causes found, 253 tasks | 6.3% (no memory) | 35.2% | +28.9 [+20.8, +37.6] |
| [DolphinBench](#dolphinbench) | Assistant requests done right, 480 held-out tests | 62.9% (compiled procedures) | 72.3% | +9.4 [+5.2, +13.5] |
| [CRMArena-Pro](#crmarena-pro) | CRM questions answered with SQL, 330 | 57.6% (no notes) | 76.7% | +19.1 [+14.5, +23.6] |
| [LongMemEval-S](#longmemeval-s) | Questions about a user's past chats, 399 | 57.4% (generic facts) | 78.7% | +21.3 [+16.3, +26.8] |
| [SWE-ContextBench](#swe-contextbench) | Software issues resolved, 217 | 27.6% (no memory) | 30.6% | +3.0 [+0.5, +6.0] |
| [tau2-bench banking](#tau2-bench-banking) | Bank support tasks passed, 97 × 3 | 27.8% (every policy as files) | 30.9% | +3.1 [−3.4, +10.0] |
| [Code review](#code-review) | Regressions caught, 688 cases (internal) | 17.1% (diff alone) | 20.3% | +3.2 [+0.7, +5.6] at 98.3% |
| [Sales pipeline questions](#sales-pipeline-questions) | Judged answer score, 69 questions (internal) | 0.714 (earlier search system) | 0.915 | +0.20 [+0.13, +0.27] |

## What every run shares

- **Arms.** A run compares arms on the same tasks. The agent, prompt and tools stay the same; only what memory
  serves changes. The usual arms are:
  - no memory, or the benchmark's own way of reading the material (files, a shell, the whole history);
  - OpenViking, an open-source agent memory;
  - AmpUp Memory search with a procedure compiled from documents before any runs;
  - AmpUp Memory search with the note learned from earlier graded runs of that kind of task.
- **Practice and test never overlap.** Notes are learned only from practice runs that share nothing with a scored
  task. Each section says how its benchmark was split.
- **Notes.** Claude Opus 5.5 learned the notes from practice runs and their grades. `get_advice` matched each task
  to its note before the run, using Claude Haiku 5.5.
- **The evidence gate was off.** The runs served every learned note (`AMPUP_MEMORY_ADVICE_EVIDENCE=0`). The product
  default serves a note outside its trial arm only after it has helped 3 people, and that default is not what these
  runs measured.
- **Statistics.** Differences are paired by task. Intervals are 95% paired bootstrap intervals over tasks, or over
  question groups where tasks share a question. A bootstrap with another seed moves an interval's ends by a few
  tenths of a point.
- **Cost.** Cost covers the agent's tokens at list price. It excludes judging, building the memory and learning the
  notes.
- **Not published here.**
  - Per-task results and full traces. They could be attached later to a GitHub release of this repository.
  - The evaluation harness that drove the runs, which is not public yet.

## ORCA-bench

**What it is.** [ORCA-bench](https://github.com/ORCA-bench/ORCA-bench) (Apache-2.0) simulates on-call
root-cause analysis on the OpenTelemetry demo shop, with faults injected through feature flags.

- **Task.** The agent gets a shell, the shop's source code, Grafana telemetry and a short complaint. It writes a
  report naming the root cause, or an empty report if nothing broke.
- **Data.** 293 tasks from six days of one shop: 132 Medium and 121 Hard incident tasks (50 questions about 13
  incidents) and 40 healthy controls.

**Setup.**

- **Agent.** Claude Haiku 5.5 at temperature 1, in the terminus-2 agent, with a 150-step cap. One run per task
  and arm.
- **Practice.** 454 graded runs of the same agent with no memory.
- **Split.** The six days are dealt into two groups of three. A test task reads only notes learned from the other
  group's runs (216 or 238 of them), so practice and test never share an incident.
- **Arms.**
  - no memory;
  - OpenViking 0.4.23 search over the 464-file corpus, with and without what its own trainer learned from the same
    practice runs;
  - AmpUp Memory passage search plus a procedure compiled from documents before any runs;
  - AmpUp Memory search plus the learned note, with and without a goal.

**Scoring.**

- **Grader.** The benchmark's own rubric judge, on GPT-6.1 Sol at high effort.
- **Solved.** A task is solved when the report names every injected root cause.
- **Invented.** A report that names none of them, but isn't empty, counts as invented.
- **Scope.** Accuracy is over the 253 incident tasks; the controls count false alarms.
- **Intervals.** Resampled by question.

**Results.** 253 incident tasks; each arm compared with no memory.

| Arm | Root causes found | vs no memory | Cost per task |
|---|---|---|---|
| No memory | 6.3% (16/253) | | $0.032 |
| OpenViking search | 9.1% | +2.8 [−0.8, +7.3] | $0.030 |
| OpenViking, with its own learning | 11.1% | +4.7 [+0.0, +10.3] | $0.033 |
| AmpUp Memory, procedure only (no runs yet) | 10.3% | +4.0 [+0.4, +8.5] | |
| AmpUp Memory notes | **35.2%** (89/253) | **+28.9 [+20.8, +37.6]**, 75 won / 2 lost | **$0.024** |
| AmpUp Memory notes, with a goal | 32.8% | +26.5 [+16.2, +36.5] | $0.025 |

- **Invented root causes.** 28.5% with no memory, 7.1% with notes.
- **Medium tasks.** 11.4% with no memory, 53.0% with notes.
- **Hard tasks.** 0.8% with no memory, 15.7% with notes and 17.4% with notes and a goal.
- **False alarms on the 40 healthy controls.** 22.5% with no memory, 17.5% with notes.
- **More practice, better notes.** Every point below is on the same 253 tasks.

  | Practice behind the notes | Root causes found |
  |---|---|
  | No memory | 6.3% |
  | Procedure only | 10.3% |
  | Notes after day 1 (135 runs) | 16.2% |
  | Notes after day 2 (284 runs) | 19.8% |
  | Notes after day 3 (454 runs) | 35.2% |

  The day-1 and day-2 points come from a separate run.
- **Against a larger model.** On ORCA-bench's mini set (the 132 Medium tasks, same grader):
  - Claude Haiku 5.5 with notes learned from all 454 practice runs: 55.3% at $0.021 a task.
  - Claude Sonnet 5.5 alone: 40.2% at $0.517 a task.
  - Difference: +15.2 [−3.0, +32.9]. The two arms ran in different sessions.

**Read with it.**

- **Practice grades named the answers.** A practice run's grade listed the expected root causes.
- **One shop.** All six days are one shop, so this measures learning from a system's own past incidents, not
  transfer to a new system.

## DolphinBench

**What it is.** [DolphinBench](https://github.com/mem0ai/dolphinbench) is Mem0's public agent-memory benchmark
(Apache-2.0).

- **Task.** An assistant acts for one person through simulated email, Slack, calendar and CRM tools, carrying out
  dated requests.
- **Data.** Three people, each with about 500,000 tokens of history over five years, and 200 requests per person.

**Setup.**

- **Agent.** Claude Haiku 5.5 at high effort, through Claude Code.
- **Memory.** Claude Haiku 5.5 extracted the facts. Claude Opus 5.5 compiled the procedures and wrote the notes.
- **Split.** Per person: 20 practice, 20 validation and 160 test requests, so 480 test requests in all. Requests
  that depend on the same source message stay on the same side.
- **Learning rounds.** Notes were learned in rounds. A round was kept only if it beat the best so far on the 60
  validation requests. Round 4 learned from 180 practice runs.
- **Arms.**
  - the history as monthly files, read with a shell;
  - AmpUp Memory search with source lookup and 8 prefetched facts;
  - search plus procedures compiled from the history;
  - search, procedures and the learned note;
  - Claude Sonnet 5.5 reading the files.

**Scoring.**

- **Grader.** DolphinBench's own checks on the log of tool calls. GPT-6 Sol judges the meaning-based checks.
- **Passing.** A request passes only if every check passes.

**Results.** 480 held-out tests.

| Arm | Done right | vs procedures alone | Cost per task |
|---|---|---|---|
| History as files | 47.0% | | $0.0073 |
| AmpUp Memory search | 57.9% | | $0.0073 |
| Search + compiled procedures | 62.9% | | $0.0370 |
| Search + procedures + learned notes | **72.3%** | **+9.4 [+5.2, +13.5]**, 77 won / 32 lost | $0.0373 |
| Claude Sonnet 5.5 reading the files | 81.4% | | $0.1187 |

- **Against a larger model.** Haiku with the learned notes and AmpUp Memory's current search scored 81.2% at
  $0.0080 a task. Sonnet 5.5 reading the files scored 81.4% at $0.1187. The difference is −0.2 [−4.5, +4.1].
- **The current search.** It came from fixes to AmpUp Memory's search between the two runs:
  - recency is measured from the request's date;
  - search no longer claims a name is missing when it isn't;
  - note routing is sturdier;
  - search returns source passages that no fact cites.

  None of the fixes changed the memory, the notes or the model.

**Read with it.**

- **Costs are list prices.** The runs used a Claude Code subscription, so costs are the agent's tokens priced at
  the Claude API list price.
- **Refused shell commands.** Some shell commands in the files arm were refused, so 47.0% understates reading
  files. Sonnet's 81.4% is somewhat low for the same reason.
- **The search fixes were found by reading test failures.**
- **Mem0 was not run as an arm.**

## CRMArena-Pro

**What it is.** [CRMArena-Pro](https://github.com/SalesforceAIResearch/CRMArena) from Salesforce AI Research.
Its data is CC BY-NC 4.0, so no task content is copied here.

- **Task.** A SQL agent answers business questions about a simulated B2B Salesforce company (27 record types,
  29,221 records) while following a written policy.

**Setup.**

- **Questions.** 22 kinds of question. Within each kind, the first 30 are practice (660 in all) and the next 15
  are scored (330 in all).
- **What memory saw.** Each practice run, its grade, and the right answer when the run was wrong.
- **Agent.** Claude Haiku 5.5, with the schema and one read-only SQL tool, for up to 30 turns. One run per
  question.
- **Notes.** Claude Opus 5.5 wrote them.
- **Arms.**
  - SQL only;
  - SQL plus one `get_advice` call first, which returns the learned note for that kind of question.

**Scoring.**

- **Automatic.** There is no model judge.
- **Correct.** An answer is right when its token F1 against the right answer is at least 0.5.
- **Exact match.** Also reported.

**Results.** 330 scored questions.

| Arm | Right | vs SQL only | Exact match | Steps per question | Cost, all 330 |
|---|---|---|---|---|---|
| SQL only | 57.6% | | 51.8% | 5.1 | $2.18 |
| SQL + AmpUp Memory notes | **76.7%** | **+19.1 [+14.5, +23.6]**, 68 won / 5 lost | 70.9% | 3.9 | $1.10 |

**Read with it.**

- **Grader conventions.** Part of the gain is the notes learning the grader's conventions, not only how the data
  is kept.

## LongMemEval-S

**What it is.** [LongMemEval](https://github.com/xiaowu0162/LongMemEval) (MIT), in its cleaned LongMemEval-S
set.

- **Task.** One model call with no tools. The model reads what memory gives it, plus a dated question about the
  user's past chats, and answers briefly.
- **Data.** Each question has its own history of about 48 sessions, about 115,000 tokens.

**Setup.**

- **Questions.** 399 sealed test questions:
  - 106 on time-based reasoning;
  - 106 across sessions;
  - 62 on facts that changed;
  - 56 on what the user said;
  - 45 on what the assistant said;
  - 24 on preferences.
- **Model.** GPT-5.6 Luna extracted the facts and answered in every arm.
- **Extraction instructions.** Written on 101 separate development questions.
- **No learning from runs.** This tests the facts memory keeps, not notes.
- **Arms.**
  - the whole history;
  - the 12 best sessions, as their best 1,500-character windows;
  - the top 40 facts from extraction with no goal, with up to 3 source excerpts;
  - the same, from extraction under one goal: "remember a user across about 50 sessions: what they said,
    preferred, did and changed, as of the question's date".

**Scoring.** LongMemEval's official judge prompt and model, GPT-4o (`gpt-4o-2024-08-06`).

**Results.** 399 test questions.

| Arm | Right | Median tokens | Cost, all 399 |
|---|---|---|---|
| The whole history | 77.7% | 103,582 | $10.53 |
| Best passages | 59.9% | | |
| Generic facts | 57.4% | 2,843 | |
| Goal facts | **78.7%**, +1.0 [−3.8, +5.8] vs the whole history | **2,640** | **$0.46** |

- **Goal facts against the other arms.** +18.8 [+13.5, +24.1] against passages, and +21.3 [+16.3, +26.8] against
  generic facts.
- **Cost per question.** About $0.0012 for goal facts. Generic facts cost about 3% more.

**Read with it.**

- **Extraction isn't in the costs.** The costs cover answering only. Extraction costs about $0.0026 a session.
- **One question per history.** Each history is asked one question, so on this benchmark extracting costs more
  than reading the whole history.
- **A stronger reader.** In a separate run with Claude Haiku 5.5 reading, the whole history scored 93.0%, against
  58–61% for the memory arms that served passages.

## SWE-ContextBench

**What it is.** [SWE-ContextBench](https://github.com/jiayuanz3/SWEContextBench) is built on real GitHub issues.
Its repository states no license, so nothing is copied here.

- **Task.** Write a patch for an issue, starting from the repository at its base commit, with no git history or
  web access, within 900 seconds.
- **Data.** 357 target issues across 40 repositories, plus a history of 1,006 earlier tasks.

**Setup.**

- **Agent.** Claude Haiku 5.5 at medium effort, driven by the Codex CLI 0.161.0. Two runs per issue and arm.
- **Practice.** One Haiku run on each history task. 739 of the 1,006 resolved.
- **Notes.** Claude Opus 5.5 wrote them.
- **Which issues count.** 137 targets were fixed by the same pull request a history task is named after, so they
  were left out. That leaves 220, of which 217 were scored.
- **Leak scan.** A scan removed any note holding a target's id or code from its fix.
- **Arms.**
  - no memory;
  - OpenViking search, with and without its own learning;
  - AmpUp Memory search plus the learned note, with and without a goal;
  - Claude Sonnet 5.5 alone, from a separate session.
- **No compiled procedures** were used on this benchmark.

**Scoring.**

- **Grader.** The benchmark's tests, from upstream SWE-ContextBench at commit `12ad6ab`.
- **Resolved.** The failing tests now pass and the passing tests still pass.
- **Infrastructure failures.** A run that failed for infrastructure is left out of every arm.

**Results.** 217 scored issues, two runs each.

| Arm | Resolved | vs no memory |
|---|---|---|
| No memory | 27.6% | |
| OpenViking search | 29.7% | +2.1 |
| OpenViking, with its own learning | 29.3% | |
| AmpUp Memory notes, with a goal | 30.0% | +2.3 [−0.5, +5.3] |
| AmpUp Memory notes | **30.6%** | **+3.0 [+0.5, +6.0]**, 18 won / 7 lost |
| Claude Sonnet 5.5 alone | 35.5% | |

- **Cost per run.** These costs are measured over all 354 scored targets, not just the 217:
  - $0.020 with no memory;
  - $0.023 with notes;
  - $0.163 for Sonnet 5.5.

  On the 217 issues alone they are $0.022, $0.025 and $0.178.
- **All 354 scored targets.** 34.2% resolved with no memory, 36.2% with notes.

**Read with it.**

- **A narrow gain.** The interval only just excludes zero.
- **A larger model does better.** Sonnet 5.5 alone beats Haiku with notes by about 5 points, at about 7 times the
  cost.

## tau2-bench banking

**What it is.** [τ²-bench](https://github.com/sierra-research/tau2-bench) from Sierra Research (MIT), in its
banking knowledge domain.

- **Task.** A support agent serves simulated bank customers. It must answer from 698 policy documents and act
  through the bank's tools.

**Setup.**

- **Tasks.** 97, each run 3 times per arm.
- **Agent.** Claude Haiku 5.5.
- **Customer.** GPT-5.2 at temperature 0 plays the customer.
- **Split.** Tasks are grouped by the documents they need: 52 groups in 3 folds. Each fold's notes learn only from
  the other folds' practice, 592 Haiku conversations on 74 tasks in all.
- **What practice grades showed.** They listed the expected actions that weren't done, with names, ids and amounts
  removed.
- **Leak scan.** A scan of 376 task-specific strings found none in memory.
- **Procedures and notes.** Claude Opus 5.5 compiled the procedures and wrote the notes.
- **Arms.**
  - all documents as files, read with a shell;
  - OpenViking 0.4.23 search, with and without its own learning;
  - AmpUp Memory search plus a procedure compiled before any practice;
  - AmpUp Memory search plus the learned note, with and without a goal.

**Scoring.**

- **Grader.** τ²'s programmatic checks on the bank's final database and the expected actions, averaged over the 3
  runs.

**Results.** 97 tasks × 3 runs; each arm compared with the files arm.

| Arm | Passed | vs files | Cost per conversation |
|---|---|---|---|
| All documents as files | 27.8% | | $0.054 |
| OpenViking search | 20.3% | −7.6 [−13.7, −2.1] | |
| OpenViking, with its own learning | 21.6% | −6.2 | |
| AmpUp Memory, procedure only | 21.0% | −6.9 | |
| AmpUp Memory notes, with a goal | 28.2% | +0.3 [−4.8, +5.8] | $0.070 |
| AmpUp Memory notes | **30.9%** | **+3.1 [−3.4, +10.0]** | $0.069 |

- **Notes against the procedures they started from.** +7.2 [+2.1, +12.7] with a goal, +10.3 [+4.1, +17.2]
  without.
- **Hard tasks.** On 23 hard tasks, every arm scores 0–3%.

**Read with it.**

- **A tie.** The notes tied reading every document; the gain is within noise.
- **Procedures alone fell behind.**

## Code review

**What it is.** An internal evaluation on AmpUp's own repository and conversations. Its data is private, so only
the method and results are given here.

- **Question.** Would AmpUp's code review bot have flagged a bug before it shipped?
- **Test cases.** Each case pairs a pull request that introduced a bug with the later pull request that fixed it.
  The bot reviews the buggy pull request as it stood before merge.

**Setup.**

- **Cases.** 688 regression pairs, with the buggy pull request merged between March and September 2026.
- **How pairs were found.** Git blame on the fix, then a model check of each pair. A blind hand check found 28 of
  30 pairs real.
- **Sealed.** The case list and the analysis plan were fixed and hashed before any review ran.
- **Reviewer.** AmpUp's production review prompt on GPT-6.1 Sol. Each pull request was reviewed twice per arm,
  9,632 reviews in all.
- **Memory.** Merged pull requests, Slack threads and meeting transcripts from September 2025 to October 2026.
  Memory is searched as of the moment the buggy pull request merged. A scan of all evidence items found nothing
  newer, and nothing from the pull request or its fix.
- **Arms.**
  - the diff alone;
  - AmpUp Memory evidence: six searches, up to 40 facts and 8 passages, with the original embedder and with the
    production embedder;
  - a placebo with another pull request's evidence of the same length;
  - learned notes;
  - a placebo note;
  - the bot's earlier advice.

**Scoring.**

- **Judging.** Three GPT-6 Sol passes read each review beside the fix.
- **A catch.** At least two of the three passes agree the review flagged the bug.
- **Intervals.** Paired bootstrap intervals over pull requests at 98.3%, Holm-corrected for three memories tested
  at once.

**Results.** Share of the 688 regressions flagged.

| Arm | Caught | Interval (98.3%) |
|---|---|---|
| AmpUp Memory evidence, production embedder | **20.3%** | **+3.2 [+0.7, +5.6]** vs the diff alone; +5.0 [+2.8, +7.3] vs the placebo |
| AmpUp Memory evidence, original embedder | 19.8% | |
| The bot's earlier advice | 17.4% | |
| The diff alone | 17.1% | |
| Placebo note | 16.6% | |
| Learned notes | 16.1% | −1.0 [−2.3, +0.2] vs the diff alone |
| Placebo evidence, same size | 15.3% | |

**Read with it.**

- **Unrelated evidence lowers catches.** What helps is what the evidence says, not how much there is.
- **Learned notes didn't improve catches.**

## Sales pipeline questions

**What it is.** An internal evaluation on AmpUp's own CRM data, synced from HubSpot as of October 12, 2026. Its
data is private, so only the method and results are given here.

- **Task.** Answer a question about the sales pipeline in about six tool calls.
- **Questions.** 69 questions of 17 kinds: 47 with exact answers and 22 judged against a rubric.

**Setup.**

- **No learning.** Nothing is learned from these questions. This tests the facts memory keeps.
- **Agents.** Claude agents answered.
- **Arms.**
  - an earlier Elasticsearch-based system, doing keyword and meaning search over snippets of about 1,500
    characters, with no record tables;
  - AmpUp Memory, with typed CRM facts from the sales schema plus the same snippets as passages;
  - both together.

**Scoring.**

- **Judge.** A model judge with a rubric (1, 0.75, 0.5 or 0), in two blind passes that were averaged. Answers were
  shuffled and anonymised.
- **Agreement.** The two passes agreed on 88% of items.
- **False claims.** An answer has a false claim if either pass found a contradiction.

**Results.** 69 questions.

| Arm | Judged score | vs the earlier system | Answers with a false claim |
|---|---|---|---|
| Earlier Elasticsearch-based system | 0.714 | | 16 |
| AmpUp Memory typed facts plus passages | **0.915** | **+0.20 [+0.13, +0.27]**, 38 better / 3 worse | 5 |
| Both together | 0.891 | | 7 |

**Read with it.**

- **An uneven comparison.** The earlier system had no structured CRM tables indexed.
