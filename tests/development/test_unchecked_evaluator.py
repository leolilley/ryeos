"""The shipped no-checks candidate evaluator refuses unless told otherwise."""

import importlib.util
import tempfile
import unittest
from pathlib import Path

TOOL = (
    Path(__file__).resolve().parents[2]
    / "bundles/core/.ai/tools/ryeos/core/evaluate/unchecked.py"
)
spec = importlib.util.spec_from_file_location("unchecked_evaluator", TOOL)
unchecked = importlib.util.module_from_spec(spec)
spec.loader.exec_module(unchecked)

CANDIDATE = "a" * 64
BASE = "b" * 64


def request(**extra):
    return {"candidate_snapshot_hash": CANDIDATE, "base_snapshot_hash": BASE, **extra}


class UncheckedEvaluator(unittest.TestCase):
    def setUp(self):
        self.view = tempfile.TemporaryDirectory()
        self.addCleanup(self.view.cleanup)
        self.project = Path(self.view.name)

    def test_it_refuses_by_default_and_says_why(self):
        result = unchecked.evaluate(request(), self.project)
        self.assertIs(result["accepted"], False)
        self.assertEqual(result["evidence"]["reason"], "no checks configured")
        self.assertEqual(result["evidence"]["scope"], "no checks run")

    def test_it_accepts_only_on_an_explicit_true(self):
        for not_true in (False, None, "true", 1, "yes"):
            result = unchecked.evaluate(request(accept_unchecked=not_true), self.project)
            self.assertIs(result["accepted"], False, not_true)
        result = unchecked.evaluate(request(accept_unchecked=True), self.project)
        self.assertIs(result["accepted"], True)
        self.assertEqual(result["evidence"]["scope"], "no checks run")
        self.assertIs(result["evidence"]["accept_unchecked"], True)

    def test_it_never_accepts_a_candidate_that_is_not_a_distinct_readable_generation(self):
        same = {"candidate_snapshot_hash": CANDIDATE, "base_snapshot_hash": CANDIDATE}
        self.assertIs(
            unchecked.evaluate({**same, "accept_unchecked": True}, self.project)["accepted"],
            False,
        )
        malformed = {"candidate_snapshot_hash": "NOT-A-HASH", "base_snapshot_hash": BASE}
        self.assertIs(
            unchecked.evaluate({**malformed, "accept_unchecked": True}, self.project)["accepted"],
            False,
        )
        absent = self.project / "missing"
        self.assertIs(
            unchecked.evaluate(request(accept_unchecked=True), absent)["accepted"], False
        )

    def test_the_result_has_exactly_the_shape_qualification_reads(self):
        result = unchecked.evaluate(request(accept_unchecked=True), self.project)
        self.assertEqual(
            sorted(result),
            ["accepted", "base_snapshot_hash", "candidate_snapshot_hash", "evidence", "schema_version"],
        )
        self.assertEqual(result["schema_version"], 1)
        self.assertEqual(result["candidate_snapshot_hash"], CANDIDATE)
        self.assertEqual(result["base_snapshot_hash"], BASE)
        self.assertIsInstance(result["evidence"], dict)


if __name__ == "__main__":
    unittest.main()
