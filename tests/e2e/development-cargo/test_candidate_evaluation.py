"""Source assertion tests: no Cargo, model, installed Tool or node execution.

Only two intentional E2E source states are supported: the exact admitted base
(synthesize the amendment in a temporary tree) and the exact intended candidate
(copy unchanged). These fixture states are not a runtime compatibility path.
The actual repository Rust file is never changed by this test.
"""

import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import yaml


ROOT = Path(__file__).resolve().parents[3]
TOOL = ROOT / ".ai/tools/ryeos/development/repository-validation/candidate.py"
CONFIG = ROOT / ".ai/config/development/ryeos/candidate-evaluation.yaml"
TASK = "strict-json-escaped-keys"
spec = importlib.util.spec_from_file_location("candidate_assertion", TOOL)
candidate = importlib.util.module_from_spec(spec)
spec.loader.exec_module(candidate)


def config():
    raw = CONFIG.read_text()
    return json.loads(raw.split("\n", 1)[1] if raw.startswith("# ryeos:signed:") else raw)


class CandidateEvaluationTests(unittest.TestCase):
    def fixture(self, root):
        configured = config()
        for assertion in configured["tasks"][TASK]["file_assertions"]:
            path = root / assertion["path"]
            path.parent.mkdir(parents=True, exist_ok=True)
            data = (ROOT / assertion["path"]).read_bytes()
            if path.name == "lib.rs":
                observed = hashlib.sha256(data).hexdigest()
                base = "def27d76db4f115f7960eb1cd0719a0714c8daa64e0a4464c517a3c7459bc50a"
                self.assertIn(observed, {base, assertion["sha256"]}, "unexpected source is not a qualification fixture")
                if observed == base:
                    addition = (Path(__file__).parent / "strict-json-key-amendment.txt").read_bytes()
                    anchor = b"    #[test]\n    fn strict_json_rejects_duplicate_keys_at_every_depth()"
                    self.assertEqual(data.count(anchor), 1)
                    data = data.replace(anchor, addition + b"\n" + anchor)
            self.assertEqual(hashlib.sha256(data).hexdigest(), assertion["sha256"])
            path.write_bytes(data)
        return configured

    def request(self, configured):
        return {"task": TASK, "base_snapshot_hash": "a" * 64,
                "candidate_snapshot_hash": "b" * 64, "resolved_config": configured}

    def test_exact_candidate_is_accepted_without_claiming_tests(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            request = self.request(self.fixture(root))
            before = {p: p.read_bytes() for p in root.rglob("*") if p.is_file()}
            result = candidate.evaluate(root, request)
            self.assertTrue(result["accepted"])
            self.assertEqual(result["base_snapshot_hash"], "a" * 64)
            self.assertEqual(result["candidate_snapshot_hash"], "b" * 64)
            self.assertEqual(result["schema_version"], 1)
            self.assertEqual(set(result["evidence"]), {"task", "files", "scope"})
            self.assertIn("no Cargo execution", result["evidence"]["scope"])
            self.assertEqual(before, {p: p.read_bytes() for p in root.rglob("*") if p.is_file()})

    def test_changed_missing_symlink_oversized_and_same_snapshot_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            request = self.request(self.fixture(root))
            request["candidate_snapshot_hash"] = request["base_snapshot_hash"]
            self.assertFalse(candidate.evaluate(root, request)["accepted"])
            request["candidate_snapshot_hash"] = "b" * 64
            path = root / "Cargo.toml"
            original = path.read_bytes()
            for data in [b"changed", b"x" * 1048577]:
                path.write_bytes(data)
                self.assertFalse(candidate.evaluate(root, request)["accepted"])
            path.unlink()
            self.assertFalse(candidate.evaluate(root, request)["accepted"])
            other = root / "elsewhere"
            other.write_bytes(original)
            path.symlink_to(other)
            self.assertFalse(candidate.evaluate(root, request)["accepted"])

    def test_no_implicit_task_config_coordinate_or_path(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            request = self.request(self.fixture(root))
            for field in ("task", "resolved_config", "base_snapshot_hash", "candidate_snapshot_hash"):
                altered = copy.deepcopy(request)
                del altered[field]
                with self.assertRaises((KeyError, ValueError)):
                    candidate.evaluate(root, altered)
            for path in ("../Cargo.toml", "/Cargo.toml", "crates/../Cargo.toml"):
                altered = copy.deepcopy(request)
                altered["resolved_config"]["tasks"][TASK]["file_assertions"][0]["path"] = path
                with self.assertRaises(ValueError):
                    candidate.evaluate(root, altered)
            for assertions in ([], [{"path": "Cargo.toml", "sha256": "a" * 64}] * 2):
                altered = copy.deepcopy(request)
                altered["resolved_config"]["tasks"][TASK]["file_assertions"] = assertions
                with self.assertRaises(ValueError):
                    candidate.evaluate(root, altered)

    def test_exact_bytes_and_aggregate_read_budget(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            request = self.request(self.fixture(root))
            config_value = request["resolved_config"]
            # Binary-open UTF8 decoding does not normalize CRLF.
            path = root / "Cargo.toml"
            path.write_bytes(b"first\r\nsecond\r\n")
            config_value["tasks"][TASK]["file_assertions"][1]["sha256"] = hashlib.sha256(path.read_bytes()).hexdigest()
            self.assertTrue(candidate.evaluate(root, request)["accepted"])
            config_value["limits"]["max_total_bytes"] = 7
            with patch.object(candidate, "read", wraps=candidate.read) as observed:
                self.assertFalse(candidate.evaluate(root, request)["accepted"])
                self.assertEqual(observed.call_count, 1)
                self.assertEqual(observed.call_args.args[2], 7)

    def test_descriptor_has_no_phase_stdout_or_worker_grant(self):
        header = []
        started = False
        for line in TOOL.read_text().splitlines():
            if line == "# ryeos-tool:":
                started = True
                continue
            if started and line.startswith("# "):
                header.append(line[2:])
            elif started:
                break
        definition = yaml.safe_load("\n".join(header))
        schema = definition["config_schema"]
        self.assertFalse(schema["additionalProperties"])
        self.assertEqual(set(schema["properties"]), {"task", "base_snapshot_hash", "candidate_snapshot_hash"})
        self.assertEqual(definition["config_resolve"]["spec"]["path"], "development/ryeos/candidate-evaluation.yaml")
        worker = yaml.safe_load((ROOT / ".ai/config/development/ryeos/worker-environment.yaml").read_text())
        self.assertNotIn("tool:ryeos/development/repository-validation/candidate",
                         [entry["item_ref"] for entry in worker["workload_client"]["executions"]])


if __name__ == "__main__":
    unittest.main()
