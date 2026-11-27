"""The declared-checks candidate evaluator: only declared checks accept, and the candidate cannot bend them."""

import hashlib
import importlib.util
import os
import tempfile
import unittest
from pathlib import Path

TOOL = (
    Path(__file__).resolve().parents[2]
    / "bundles/core/.ai/tools/ryeos/core/evaluate/declared.py"
)
spec = importlib.util.spec_from_file_location("declared_evaluator", TOOL)
declared = importlib.util.module_from_spec(spec)
spec.loader.exec_module(declared)

CANDIDATE = "a" * 64
BASE = "b" * 64
DIGEST = "c" * 64
REF = "config:ryeos/core/evaluate/checks"


def run(project, checks, digest=DIGEST):
    return declared.evaluate(
        {
            "candidate_snapshot_hash": CANDIDATE,
            "base_snapshot_hash": BASE,
            "base_config": base_config(checks, digest),
        },
        project,
    )


def base_config(checks, digest=DIGEST, trusted=True, reason=None):
    if checks is None:
        return {"ref": REF, "trusted": False, "digest": None, "value": None, "reason": reason or "absent"}
    return {"ref": REF, "trusted": trusted, "digest": digest, "value": {"checks": checks}, "reason": reason}


def outcome(result, check_id):
    return next(c for c in result["evidence"]["checks"] if c["id"] == check_id)


class DeclaredEvaluator(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / "src").mkdir()
        (self.root / "src/a.txt").write_bytes(b"alpha")
        (self.root / "data.json").write_text('{"k": [1, 2]}')
        (self.root / "conf.yaml").write_text("a: 1\nb: [x, y]\n")

    def test_no_checks_declared_never_accepts(self):
        for checks in (None, []):
            result = run(self.root, checks)
            self.assertIs(result["accepted"], False)
            self.assertEqual(result["evidence"]["scope"], "no checks declared")
            self.assertEqual(result["evidence"]["checks_declared"], 0)

    def test_a_config_that_is_not_well_formed_declares_nothing(self):
        bad = [
            [{"id": "x", "kind": "unknown"}],
            [{"id": "X", "kind": "path_exists", "path": "src/a.txt"}],
            [{"id": "x", "kind": "path_exists", "path": "a", "extra": 1}],
            [
                {"id": "x", "kind": "path_exists", "path": "src/a.txt"},
                {"id": "x", "kind": "path_exists", "path": "src/a.txt"},
            ],
        ]
        for checks in bad:
            result = run(self.root, checks)
            self.assertIs(result["accepted"], False, checks)
            self.assertEqual(result["evidence"]["checks_declared"], 0)
        no_digest = declared.evaluate(
            {
                "candidate_snapshot_hash": CANDIDATE,
                "base_snapshot_hash": BASE,
                "base_config": {"ref": REF, "trusted": True, "digest": None, "value": {"checks": [{"id": "x", "kind": "path_exists", "path": "data.json"}]}},
            },
            self.root,
        )
        self.assertIs(no_digest["accepted"], False)

    def test_path_exists_and_absent(self):
        result = run(
            self.root,
            [
                {"id": "has", "kind": "path_exists", "path": "src/a.txt"},
                {"id": "gone", "kind": "path_absent", "path": "src/b.txt"},
            ],
        )
        self.assertIs(result["accepted"], True)
        self.assertEqual(result["evidence"]["config_digest"], DIGEST)
        self.assertEqual(result["evidence"]["scope"], "declared checks run")
        failing = run(
            self.root,
            [
                {"id": "missing", "kind": "path_exists", "path": "src/nope.txt"},
                {"id": "present", "kind": "path_absent", "path": "src/a.txt"},
            ],
        )
        self.assertIs(failing["accepted"], False)
        self.assertEqual(outcome(failing, "missing")["result"], "fail")
        self.assertEqual(outcome(failing, "present")["result"], "fail")

    def test_content_sha256(self):
        good = hashlib.sha256(b"alpha").hexdigest()
        self.assertIs(
            run(self.root, [{"id": "h", "kind": "content_sha256", "path": "src/a.txt", "sha256": good}])["accepted"],
            True,
        )
        self.assertIs(
            run(self.root, [{"id": "h", "kind": "content_sha256", "path": "src/a.txt", "sha256": "0" * 64}])["accepted"],
            False,
        )

    def test_paths_within_allow_and_deny(self):
        spec = {"id": "p", "kind": "paths_within", "allow": ["src/**", "*.json", "*.yaml"], "deny": []}
        self.assertIs(run(self.root, [spec])["accepted"], True)
        (self.root / "outside.bin").write_bytes(b"x")
        refused = run(self.root, [spec])
        self.assertIs(refused["accepted"], False)
        self.assertIn("outside the allow list", outcome(refused, "p")["detail"])
        (self.root / "outside.bin").unlink()
        denied = run(self.root, [{**spec, "deny": ["src/*.txt"]}])
        self.assertIs(denied["accepted"], False)
        self.assertIn("deny list", outcome(denied, "p")["detail"])

    def test_limits(self):
        ok = {"id": "l", "kind": "limits", "max_files": 3, "max_total_bytes": 1000, "max_file_bytes": 100}
        self.assertIs(run(self.root, [ok])["accepted"], True)
        for key, value in (("max_files", 2), ("max_total_bytes", 5), ("max_file_bytes", 4)):
            self.assertIs(run(self.root, [{**ok, key: value}])["accepted"], False, key)

    def test_parses_as(self):
        self.assertIs(
            run(self.root, [
                {"id": "j", "kind": "parses_as", "path": "data.json", "format": "json"},
                {"id": "y", "kind": "parses_as", "path": "conf.yaml", "format": "yaml"},
            ])["accepted"],
            True,
        )
        (self.root / "bad.json").write_text("{nope")
        self.assertIs(
            run(self.root, [{"id": "j", "kind": "parses_as", "path": "bad.json", "format": "json"}])["accepted"],
            False,
        )

    def test_parse_byte_limit_and_yaml_alias_are_refused(self):
        (self.root / "big.json").write_text('"' + "a" * 2000 + '"')
        over = run(self.root, [{"id": "j", "kind": "parses_as", "path": "big.json", "format": "json", "max_bytes": 100}])
        self.assertIs(over["accepted"], False)
        self.assertIn("larger than", outcome(over, "j")["detail"])
        bomb = "a: &a [x, x]\nb: &b [*a, *a]\nc: [*b, *b]\n"
        (self.root / "bomb.yaml").write_text(bomb)
        refused = run(self.root, [{"id": "y", "kind": "parses_as", "path": "bomb.yaml", "format": "yaml"}])
        self.assertIs(refused["accepted"], False)
        self.assertIn("aliases", outcome(refused, "y")["detail"])
        (self.root / "deep.json").write_text("[" * 100000 + "]" * 100000)
        deep = run(self.root, [{"id": "j", "kind": "parses_as", "path": "deep.json", "format": "json", "max_bytes": 1000000}])
        self.assertIs(deep["accepted"], False)

    def test_a_symlink_on_a_checked_path_fails_the_check(self):
        target = self.root / "real"
        target.mkdir()
        (target / "f.txt").write_bytes(b"alpha")
        os.symlink(target, self.root / "linkdir")
        os.symlink(self.root / "src/a.txt", self.root / "linkfile")
        for path in ("linkdir/f.txt", "linkfile"):
            result = run(self.root, [{"id": "s", "kind": "path_exists", "path": path}])
            self.assertIs(result["accepted"], False, path)
            h = hashlib.sha256(b"alpha").hexdigest()
            result = run(self.root, [{"id": "s", "kind": "content_sha256", "path": path, "sha256": h}])
            self.assertIs(result["accepted"], False, path)
        absent = run(self.root, [{"id": "a", "kind": "path_absent", "path": "linkfile"}])
        self.assertIs(absent["accepted"], False)

    def test_noncanonical_paths_fail(self):
        for path in ("../x", "/etc/passwd", "src//a.txt", "./data.json", "src/"):
            result = run(self.root, [{"id": "p", "kind": "path_exists", "path": path}])
            self.assertIs(result["accepted"], False, path)

    def test_an_untrusted_config_declares_nothing_and_says_why(self):
        untrusted = base_config(
            [{"id": "x", "kind": "path_exists", "path": "data.json"}],
            trusted=False,
            reason="the config's signer is not trusted for this project",
        )
        result = declared.evaluate(
            {"candidate_snapshot_hash": CANDIDATE, "base_snapshot_hash": BASE, "base_config": untrusted},
            self.root,
        )
        self.assertIs(result["accepted"], False)
        self.assertEqual(result["evidence"]["checks_declared"], 0)
        self.assertIn("not trusted", result["evidence"]["reason"])
        self.assertEqual(result["evidence"]["config_ref"], REF)

    def test_a_candidate_cannot_declare_its_own_checks(self):
        (self.root / ".ai/config/ryeos/core/evaluate").mkdir(parents=True)
        (self.root / ".ai/config/ryeos/core/evaluate/checks.yaml").write_text(
            "checks: [{id: x, kind: path_exists, path: data.json}]\n"
        )
        result = run(self.root, None)
        self.assertIs(result["accepted"], False)
        self.assertEqual(result["evidence"]["checks_declared"], 0)

    def test_evidence_names_digests_and_every_check(self):
        result = run(self.root, [{"id": "has", "kind": "path_exists", "path": "data.json"}])
        evidence = result["evidence"]
        self.assertEqual(evidence["config_digest"], DIGEST)
        self.assertEqual(len(evidence["checks_digest"]), 64)
        self.assertEqual(evidence["checks"], [{"id": "has", "result": "pass", "detail": "file exists"}])
        self.assertLessEqual(len(evidence["scope"]), 200)

    def test_identical_generation_is_never_accepted(self):
        result = declared.evaluate(
            {
                "candidate_snapshot_hash": CANDIDATE,
                "base_snapshot_hash": CANDIDATE,
                "base_config": base_config([{"id": "x", "kind": "path_exists", "path": "data.json"}]),
            },
            self.root,
        )
        self.assertIs(result["accepted"], False)


if __name__ == "__main__":
    unittest.main()
