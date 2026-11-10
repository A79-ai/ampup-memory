import argparse
import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import search_cpu_bench as cpu
from search_cpu_bench import canonical, summary


class SearchCpuBenchTests(unittest.TestCase):
    def test_only_snapshot_cursor_is_ignored(self):
        hit = {"fact_id": "one", "score": 0.5, "source_episodes": ["source"]}
        base = {"hits": [hit], "cursor": "a"}
        self.assertEqual(canonical(base), canonical({"cursor": "b", "hits": [hit]}))
        for changed in [{**hit, "score": 0.6}, {**hit, "source_episodes": ["other"]}]:
            self.assertNotEqual(canonical(base), canonical({"hits": [changed]}))
        self.assertNotEqual(canonical({"hits": [hit, {"fact_id": "two"}]}),
                            canonical({"hits": [{"fact_id": "two"}, hit]}))

    def test_warmups_excluded_and_paired_delta_has_expected_sign(self):
        rows = []
        for query, base in [("a", 10), ("b", 20)]:
            for arm, multiplier in [("baseline", 1), ("candidate", 0.5)]:
                for phase in ["warmup", "warm", "warm"]:
                    rows.append({"fixture": "test", "variant": "broad", "arm": arm,
                                 "query_id": query, "phase": phase,
                                 "ms": 999 if phase == "warmup" else base * multiplier,
                                 "parity": True, "evidence_hit": True})
        result = next(row for row in summary(rows, "baseline") if row["arm"] == "candidate")
        self.assertEqual(result["requests"], 4)
        self.assertEqual(result["queries"], 2)
        self.assertEqual(result["p50_ms"], 7.5)
        self.assertEqual(result["p50_change_percent"], -50)
        self.assertEqual(result["paired_bootstrap_95_percent"], [-50, -50])

    def fixture(self, root):
        """Tiny fake on-disk fixture; no executable, indexer or HTTP server may run."""
        queries = [{"id": "test/broad", "variant": "broad", "arguments": {"query": "test"},
                    "gold_episode_ids": ["source"]}]
        cpu.bench.write_json(root / "queries.json", queries)
        corpus = root / "corpora" / "test"
        corpus.mkdir(parents=True)
        for name in ["episodes", "facts"]:
            (corpus / f"{name}.jsonl").write_text("{}\n")
        hashes = {f"{name}_sha256": cpu.bench.digest(corpus / f"{name}.jsonl")
                  for name in ["episodes", "facts"]}
        cpu.bench.write_json(root / "fixture.json", {
            "queries_sha256": cpu.bench.digest(root / "queries.json"), "fixtures": {"test": hashes}})
        artifacts = {}
        for arm in ["baseline", "candidate"]:
            (root / arm).write_text(arm)
            digest = cpu.bench.digest(root / arm)
            artifacts[arm] = {"binary_sha256": digest}
            store = root / "stores" / "test" / arm
            (store / "index" / "memory_org" / "v10" / "open.lance").mkdir(parents=True)
            cpu.bench.write_json(store / "benchmark-ready.json", {
                "binary_sha256": digest, "corpus": hashes})
        cpu.bench.write_json(root / "experiment.json", {"artifacts": artifacts})
        return argparse.Namespace(root=root, warmups=1, repeats=1, reference="baseline",
                                  binary=[f"{arm}={root / arm}" for arm in artifacts],
                                  seed=436, profile="unit test", fixtures="test")

    def test_runner_rejects_result_changes_in_warmups_and_measured_passes(self):
        for changed_pass in [None, 0, 1]:
            with self.subTest(changed_pass=changed_pass), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                args = self.fixture(root)
                servers = []

                class FakeServer:
                    def __init__(self, binary, *_):
                        self.arm, self.calls, self.closed = binary.name, 0, False
                        servers.append(self)

                    def query(self, _):
                        changed = self.arm == "candidate" and self.calls == changed_pass
                        self.calls += 1
                        return 1.0, {"cursor": self.arm, "hits": [{
                            "fact_id": "one", "score": 0.6 if changed else 0.5,
                            "source_episodes": ["source"]}]}

                    def close(self):
                        self.closed = True

                with mock.patch.object(cpu.bench, "Server", FakeServer), \
                        mock.patch.object(cpu.platform, "platform", return_value="test-host"), \
                        mock.patch.object(cpu.subprocess, "run", side_effect=AssertionError("process started")), \
                        contextlib.redirect_stdout(io.StringIO()):
                    if changed_pass is None:
                        cpu.run(args)
                    else:
                        with self.assertRaisesRegex(SystemExit, "Payload parity failed"):
                            cpu.run(args)
                run = next((root / "runs").iterdir())
                provenance = json.loads((run / "provenance.json").read_text())
                self.assertEqual(provenance["status"], "complete")
                self.assertEqual(provenance["payload_parity_passed"], changed_pass is None)
                requests = cpu.bench.jsonl(run / "requests.jsonl")
                self.assertEqual(len(requests), 4)
                self.assertEqual(sum(not row["parity"] for row in requests),
                                 0 if changed_pass is None else 1)
                if changed_pass is not None:
                    suffix = "first" if changed_pass == 0 else "mismatch-1"
                    payload = json.loads((run / f"test-candidate-test-broad-{suffix}.json").read_text())
                    self.assertEqual(payload["hits"][0]["score"], 0.6)
                self.assertEqual(len(servers), 2)
                self.assertTrue(all(s.closed and s.calls == 2 for s in servers))

    def test_runner_rejects_untrusted_artifacts_before_starting_servers(self):
        cases = [
            ("duplicate", "distinct executable"),
            ("binary", "source/build provenance"),
            ("queries", "Frozen queries changed"),
            ("corpus", "Frozen test/facts changed"),
        ]
        for kind, message in cases:
            with self.subTest(kind=kind), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                args = self.fixture(root)
                if kind == "duplicate":
                    (root / "candidate").write_bytes((root / "baseline").read_bytes())
                else:
                    path = {"binary": root / "candidate", "queries": root / "queries.json",
                            "corpus": root / "corpora" / "test" / "facts.jsonl"}[kind]
                    path.write_text("changed")
                with mock.patch.object(cpu.bench, "Server") as server, \
                        mock.patch.object(cpu.platform, "platform", return_value="test-host"), \
                        mock.patch.object(cpu.subprocess, "run", side_effect=AssertionError("process started")) as process, \
                        contextlib.redirect_stdout(io.StringIO()):
                    with self.assertRaisesRegex(ValueError, message):
                        cpu.run(args)
                    server.assert_not_called()
                    process.assert_not_called()

    def test_incomplete_runs_cannot_produce_a_comparison_report(self):
        for status in ["running", "failed"]:
            with self.subTest(status=status), tempfile.TemporaryDirectory() as temp:
                root = Path(temp)
                cpu.bench.write_json(root / "provenance.json", {"status": status})
                with self.assertRaisesRegex(ValueError, "Only complete runs"):
                    cpu.report(root)
                self.assertFalse((root / "report.md").exists())


if __name__ == "__main__":
    unittest.main()
