# Retained search CPU experiments

`search_cpu_bench.py` compares independently built search variants against a single
baseline through the real local `search_memory` endpoint. It keeps the corpus,
queries, binaries, indexes, raw responses and timings so a later change can be
tested against the same inputs.

The retained fixture (`retrieval-cpu-v1`) is not shipped in this repository; pass the
directory of yours as `--root`. Its
manifest hashes two corpora and 36 query/filter combinations. The Dolphin sample
has 600 episodes and 5,112 facts. The mixed stress fixture adds 5,100 artificial
email distractors. Each of 12 questions has a broad, document-only and
document-plus-date variant. Source labels identify one known supporting episode;
they are incomplete relevance judgments.

Build each variant from the same frozen revision with identical settings. The
recorded experiment uses Lance 13 and the existing v10 fact layout, excluding the
held episode-type prefilter change. Keep executable and source/patch hashes in
`experiment.json`; the runner copies that manifest into each result directory.
Never relabel cumulative changes as isolated experiments.

Example, using the retained root and its saved executables:

```sh
python3 harness/search_cpu_bench.py run \
  --root /absolute/path/to/output/retrieval-cpu-v1 \
  --binary baseline=/absolute/path/to/bin/baseline \
  --binary candidate_reuse=/absolute/path/to/bin/candidate_reuse \
  --binary topk_selection=/absolute/path/to/bin/topk_selection \
  --binary selective_vectors=/absolute/path/to/bin/selective_vectors \
  --binary combined=/absolute/path/to/bin/combined \
  --profile 'Cargo dev: workspace opt-level=3, dependencies opt-level=1, debug assertions enabled, incremental compilation disabled' \
  --warmups 2 --repeats 8 --seed 436 --fixtures dolphin,mixed
```

Run again with a different seed and reversed fixture order. Finish builds before
timing requests. The runner records host load, but cannot isolate the machine
from other workloads. It rotates arm order, shuffles questions, excludes warmups,
and bootstraps paired per-question medians rather than treating repetitions as
independent queries. Negative latency deltas are faster.

Every response must match the baseline in content, order, scores, and citations;
only snapshot-specific pagination cursors are excluded. First-pass responses,
all mismatches, and every request's response hash are saved. Inspect both payload
parity and the separate fact-source and supplemental-evidence metrics. Equal
responses preserve behavior on this fixture; they do not prove general recall or
precision.

The default run uses cached flat-vector search, local BGE-small embeddings, two
Lance CPU threads and serial HTTP requests. It does not measure cloud storage,
ANN, concurrent production traffic or generated answers. These application-
optimized development builds are not production release qualification. The
runner disables extraction, compilation, dreaming and maintenance and uses local
stores, so the benchmark does not mutate a live organization.

`report` regenerates a completed run's report from retained raw rows:

```sh
python3 harness/search_cpu_bench.py report /absolute/path/to/run
python3 -m unittest discover -s harness -p test_search_cpu_bench.py
```

## Automated regression coverage

Every PR's CI runs the Rust search tests and the Python benchmark guardrail tests.
The Rust tests check:

- Candidate preparation occurs once per physical row across repeated keyword and
  vector lists, including rejected rows, duplicate fact IDs, missing embeddings
  and list-specific distances. Each ranking pass gets fresh preparation and keeps
  its row allocations alive until it ends.
- Top-k matches a full-sort reference exactly, including ties, zero/oversized k,
  signed zero, infinities and NaNs.
- Selective distances match full scoring bit-for-bit at both sides of the
  selectivity threshold, with nulls, sliced arrays and noncontiguous row IDs.
- Real Lance SQL filters and cached flat search preserve IDs, distances and order
  across multiple fragments, deletes, appends and reads of an older pinned version.

The Python tests use temporary fake fixtures and servers. They verify that result
changes in either warmups or measured requests fail the run and save the changed
payload; cursor-only differences pass. Changed queries/corpora, duplicate or
unrecorded binaries, and incomplete reports are rejected. They also check that
warmups do not enter latency summaries and paired deltas have the expected sign.

CI does not enforce a wall-clock latency threshold on shared runners. The
preparation-count assertion protects the repeated-work optimization; performance
comparisons still use the retained fixtures and separately recorded measurements.
