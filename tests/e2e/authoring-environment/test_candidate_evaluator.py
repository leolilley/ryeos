"""Fixture assertion tests, not installed runtime/closure qualification.

The external test harness supplies host zsh/cmp solely to check the assertion.
Installed evaluation must instead resolve the signed Tool's exact realization.
"""

import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

import yaml


TOOL = Path(__file__).parent / ".ai/tools/qualification/format-candidate.yaml"


class CandidateEvaluatorTests(unittest.TestCase):
    def evaluate(self, content, symlink=False):
        definition = yaml.safe_load(TOOL.read_text())
        script = definition["config"]["args"][2]["literal"]
        zsh, cmp = shutil.which("zsh"), shutil.which("cmp")
        self.assertIsNotNone(zsh, "external fixture test requires host zsh")
        self.assertIsNotNone(cmp, "external fixture test requires host cmp")
        with tempfile.TemporaryDirectory(prefix="ryeos-candidate-assertion.") as directory:
            root = Path(directory)
            sample = root / "crates/qualification/sample.rs"
            sample.parent.mkdir(parents=True)
            if symlink:
                target = root / "other.rs"
                target.write_text(content)
                sample.symlink_to(target)
            else:
                sample.write_text(content)
            result = subprocess.run(
                [zsh, "-f", "-c", script, "fixture-test", cmp, "a" * 64, "b" * 64],
                cwd=root, env={"PATH": ""}, check=True, timeout=5,
                capture_output=True, text=True,
            )
            self.assertEqual(sample.read_text(), content, "evaluator changed the candidate")
        value = json.loads(result.stdout)
        self.assertEqual(value["base_snapshot_hash"], "a" * 64)
        self.assertEqual(value["candidate_snapshot_hash"], "b" * 64)
        self.assertEqual(value["schema_version"], 1)
        return value

    def test_exact_formatted_candidate_is_accepted(self):
        self.assertTrue(self.evaluate("pub fn value() -> u32 {\n    2\n}\n")["accepted"])

    def test_unchanged_unformatted_and_extended_candidates_are_refused(self):
        for source in [
            "pub fn value() -> u32 {\n    1\n}\n",
            "pub fn value()->u32{2}\n",
            "pub fn value() -> u32 {\n    2\n}\n" + "x" * 1048576,
        ]:
            self.assertFalse(self.evaluate(source)["accepted"])

    def test_symlink_is_not_an_exact_regular_sample(self):
        self.assertFalse(self.evaluate("pub fn value() -> u32 {\n    2\n}\n", symlink=True)["accepted"])


if __name__ == "__main__":
    unittest.main()
