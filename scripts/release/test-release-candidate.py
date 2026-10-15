#!/usr/bin/env python3

import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
TOOL = ROOT / "scripts/release/release-candidate.py"
TRIPLE = "x86_64-unknown-linux-gnu"
BUNDLE = "web"
BINARY = "ryeos-web-tools"
COMMIT = "c" * 40
SIGNER_A = "a" * 64
SIGNER_B = "b" * 64
PUBLISHER = "d" * 64
OTHER_BUNDLE = "e" * 64


def signed(body, signer=SIGNER_A):
    digest = hashlib.sha256(body.encode()).hexdigest()
    return f"# ryeos:signed:2026-01-01T00:00:00Z:{digest}:c2ln:{signer}\n{body}"


def make_tree(base):
    bundle = base / BUNDLE
    bin_dir = bundle / ".ai/bin" / TRIPLE
    bin_dir.mkdir(parents=True)
    (bin_dir / BINARY).write_bytes(b"binary")
    (bin_dir / BINARY).chmod(0o755)
    (bin_dir / f"{BINARY}.item_source.json").write_text('{"item_ref":"x"}')
    ai = bundle / ".ai"
    (ai / "manifest.yaml").write_text(signed("name: web\n"))
    (ai / "tools").mkdir()
    (ai / "tools/a.yaml").write_text(signed("a: 1\n"))
    (ai / "tools/b.yaml").write_text(signed("b: 1\n"))
    (ai / "tools/unsigned.txt").write_text("not an item\n")
    for path in bundle.rglob("*"):
        if path.is_file() and not path.stat().st_mode & 0o111:
            path.chmod(0o644)
    interpreter = base.parent / "py"
    interpreter.write_text("#!/bin/sh\necho Python 3.0.1\n")
    interpreter.chmod(0o755)
    return bundle, bin_dir, interpreter


class ReleaseCandidateTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.work = Path(self._tmp.name)
        self.tree = self.work / "tree"
        self.tree.mkdir()
        self.bundle, self.bin, self.interp = make_tree(self.tree)

    def build_args(self, *extra):
        return ["build", "--repository-root", str(ROOT), "--tree", str(self.tree),
                "--bundle", BUNDLE, "--triple", TRIPLE, "--version", "1.0.0",
                "--source-commit", COMMIT, "--authority-class", "dev",
                "--publisher-fingerprint", PUBLISHER, "--interpreter", str(self.interp),
                "--requires-bundle", f"standard={OTHER_BUNDLE}", *extra]

    def run_tool(self, args):
        return subprocess.run([sys.executable, "-I", str(TOOL), *args],
                              text=True, capture_output=True, check=False)

    def build(self, *extra):
        result = self.run_tool(self.build_args(*extra))
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def save(self, text, name="manifest.json"):
        path = self.work / name
        path.write_text(text)
        return path

    def recompute(self, manifest, *extra, interpreter=True, bundles=True):
        args = ["recompute", "--repository-root", str(ROOT), "--tree", str(self.tree),
                "--source-commit", COMMIT, "--manifest", str(manifest)]
        if interpreter:
            args += ["--interpreter", str(self.interp)]
        if bundles:
            args += ["--bundle-digest", f"standard={OTHER_BUNDLE}"]
        result = self.run_tool([*args, *extra])
        return result.returncode, json.loads(result.stdout) if result.stdout else None, result

    def status(self, report, name):
        return report["entries"][name]["status"]

    def test_manifest_shape_and_stable_digest(self):
        first = self.build()
        self.assertEqual(first, self.build())
        record = json.loads(first)
        self.assertEqual(record["schema"], "ryeos.release_candidate.v1")
        self.assertEqual(record["authority"], {"class": "dev", "publisher_fingerprint": PUBLISHER})
        self.assertIsNone(record["predecessor"])
        self.assertEqual(sorted(record["items"]),
                         [".ai/manifest.yaml", ".ai/tools/a.yaml", ".ai/tools/b.yaml"])
        self.assertEqual(record["items"][".ai/tools/a.yaml"]["signer"], SIGNER_A)
        self.assertEqual(record["items"][".ai/tools/a.yaml"]["hash"],
                         hashlib.sha256(b"a: 1\n").hexdigest())
        self.assertEqual(record["native"]["schema"], "ryeos.core_native_set.v1")
        self.assertEqual(sorted(record["closure"]),
                         [f"bundle:standard", "interpreter", f"native:{BINARY}"])
        self.assertEqual(first, json.dumps(record, sort_keys=True, separators=(",", ":")) + "\n")
        self.assertNotIn(str(self.work), first)
        body = {k: v for k, v in record.items() if k != "digest"}
        self.assertEqual(record["digest"], hashlib.sha256(
            json.dumps(body, sort_keys=True, separators=(",", ":")).encode()).hexdigest())

    def test_malformed_envelope_refused(self):
        (self.bundle / ".ai/tools/a.yaml").write_text("# ryeos:signed:garbage\na: 1\n")
        result = self.run_tool(self.build_args())
        self.assertEqual(result.returncode, 2)
        self.assertIn("malformed signature envelope: .ai/tools/a.yaml", result.stderr)

    def test_unsigned_file_edit_is_detected(self):
        record = json.loads(self.build())
        self.assertIn(".ai/tools/unsigned.txt", record["unsigned"])
        self.assertNotIn(".ai/tools/unsigned.txt", record["items"])
        manifest = self.save(json.dumps(record, sort_keys=True, separators=(",", ":")))
        (self.bundle / ".ai/tools/unsigned.txt").write_text("not an itez\n")
        code, report, _ = self.recompute(manifest)
        self.assertEqual(code, 1)
        self.assertEqual(self.status(report, "unsigned/.ai/tools/unsigned.txt"), "changed")

    def test_malformed_manifests_refused_with_exit_two(self):
        record = json.loads(self.build())
        del record["generation_inputs"]
        for text in (json.dumps(record), "[]", "{}"):
            code, _, result = self.recompute(self.save(text))
            self.assertEqual(code, 2, text)
            self.assertNotIn("Traceback", result.stderr)
            self.assertIn("malformed manifest", result.stderr)

    def test_resign_does_not_change_hash_but_changes_signer(self):
        before = json.loads(self.build())
        (self.bundle / ".ai/tools/a.yaml").write_text(signed("a: 1\n", SIGNER_B))
        after = json.loads(self.build())
        path = ".ai/tools/a.yaml"
        self.assertEqual(before["items"][path]["hash"], after["items"][path]["hash"])
        self.assertNotEqual(before["digest"], after["digest"])

    def test_unchanged_tree_recomputes_ok_with_stable_closure_digest(self):
        manifest = self.save(self.build())
        code, report, result = self.recompute(manifest)
        self.assertEqual(code, 0, result.stdout)
        self.assertTrue(report["ok"])
        self.assertEqual(report["refused"], [])
        self.assertEqual(set(e["status"] for e in report["entries"].values()), {"match"})
        again = self.recompute(manifest)[1]
        self.assertEqual(report["closure_digest"], again["closure_digest"])

    def test_changed_item_is_named(self):
        manifest = self.save(self.build())
        (self.bundle / ".ai/tools/a.yaml").write_text(signed("a: 2\n"))
        code, report, _ = self.recompute(manifest)
        self.assertEqual(code, 1)
        self.assertEqual(self.status(report, "items/.ai/tools/a.yaml/hash"), "changed")
        self.assertEqual(self.status(report, "items/.ai/tools/b.yaml/hash"), "match")
        self.assertFalse(report["ok"])

    def test_changed_signer_is_named(self):
        manifest = self.save(self.build())
        (self.bundle / ".ai/tools/a.yaml").write_text(signed("a: 1\n", SIGNER_B))
        _, report, _ = self.recompute(manifest)
        self.assertEqual(self.status(report, "items/.ai/tools/a.yaml/signer"), "changed")
        self.assertFalse(report["ok"])

    def test_changed_binary_is_named(self):
        manifest = self.save(self.build())
        (self.bin / BINARY).write_bytes(b"other")
        _, report, _ = self.recompute(manifest)
        self.assertEqual(self.status(report, f"native/bin/{TRIPLE}/{BINARY}"), "changed")
        self.assertEqual(self.status(report, f"closure/native:{BINARY}/digest"), "changed")
        self.assertFalse(report["ok"])

    def test_changed_sidecar_is_named(self):
        manifest = self.save(self.build())
        (self.bin / f"{BINARY}.item_source.json").write_text("{}")
        _, report, _ = self.recompute(manifest)
        self.assertEqual(
            self.status(report, f"native/item_sources/{TRIPLE}/{BINARY}.item_source.json"),
            "changed")
        self.assertFalse(report["ok"])

    def test_changed_interpreter_is_named(self):
        manifest = self.save(self.build())
        self.interp.write_text("#!/bin/sh\necho Python 3.0.2\n")
        _, report, _ = self.recompute(manifest)
        self.assertEqual(self.status(report, "native/interpreter/sha256"), "changed")
        self.assertEqual(self.status(report, "native/interpreter/version"), "changed")
        self.assertEqual(self.status(report, "closure/interpreter/digest"), "changed")
        self.assertFalse(report["ok"])

    def test_changed_required_bundle_digest_is_named(self):
        manifest = self.save(self.build())
        args = ["recompute", "--repository-root", str(ROOT), "--tree", str(self.tree),
                "--source-commit", COMMIT, "--manifest", str(manifest),
                "--interpreter", str(self.interp), "--bundle-digest", f"standard={'f' * 64}"]
        result = self.run_tool(args)
        report = json.loads(result.stdout)
        self.assertEqual(result.returncode, 1)
        self.assertEqual(self.status(report, "closure/bundle:standard/digest"), "changed")

    def test_old_manifest_refuses_changed_tree_by_name(self):
        old = self.save(self.build(), "old.json")
        (self.bundle / ".ai/tools/b.yaml").write_text(signed("b: 2\n"))
        code, report, _ = self.recompute(old)
        self.assertEqual(code, 1)
        self.assertEqual(report["refused"], ["digest", "items/.ai/tools/b.yaml/hash"])

    def test_added_and_removed_items_are_reported(self):
        manifest = self.save(self.build())
        (self.bundle / ".ai/tools/c.yaml").write_text(signed("c: 1\n"))
        (self.bundle / ".ai/tools/b.yaml").unlink()
        _, report, _ = self.recompute(manifest)
        self.assertEqual(self.status(report, "items/.ai/tools/c.yaml/hash"), "added")
        self.assertEqual(self.status(report, "items/.ai/tools/b.yaml/hash"), "removed")
        self.assertFalse(report["ok"])

    def test_predecessor_diff_lists_exactly_changes(self):
        old_text = self.build()
        old = self.save(old_text, "old.json")
        old_digest = json.loads(old_text)["digest"]
        (self.bundle / ".ai/tools/a.yaml").write_text(signed("a: 2\n"))
        (self.bundle / ".ai/tools/b.yaml").write_text(signed("b: 1\n", SIGNER_B))
        new = self.save(self.build("--predecessor", old_digest), "new.json")
        code, report, result = self.recompute(new, "--predecessor-manifest", str(old))
        self.assertEqual(code, 0, result.stdout)
        diff = report["predecessor_diff"]
        self.assertEqual(diff["changed_items"], [".ai/tools/a.yaml"])
        self.assertEqual(diff["signer_changes"], [
            {"from": SIGNER_A, "path": ".ai/tools/b.yaml", "to": SIGNER_B}])
        self.assertEqual(diff["added_items"], [])
        self.assertEqual(diff["removed_items"], [])
        self.assertTrue(diff["native_digest_equal"])
        self.assertEqual(self.status(report, "predecessor"), "match")

    def test_predecessor_native_change_is_reported(self):
        old = self.save(self.build(), "old.json")
        old_digest = json.loads(old.read_text())["digest"]
        (self.bin / BINARY).write_bytes(b"other")
        new = self.save(self.build("--predecessor", old_digest), "new.json")
        _, report, _ = self.recompute(new, "--predecessor-manifest", str(old))
        self.assertFalse(report["predecessor_diff"]["native_digest_equal"])

    def test_wrong_predecessor_is_refused_and_missing_is_unavailable(self):
        old = self.save(self.build(), "old.json")
        new = self.save(self.build("--predecessor", "9" * 64), "new.json")
        code, report, _ = self.recompute(new, "--predecessor-manifest", str(old))
        self.assertEqual(code, 1)
        self.assertEqual(self.status(report, "predecessor"), "changed")
        code, report, _ = self.recompute(new)
        self.assertEqual(code, 1)
        self.assertEqual(self.status(report, "predecessor"), "unavailable")

    def test_unavailable_bundle_edge_is_never_ok(self):
        manifest = self.save(self.build())
        code, report, _ = self.recompute(manifest, bundles=False)
        self.assertEqual(code, 1)
        self.assertFalse(report["ok"])
        self.assertEqual(self.status(report, "closure/bundle:standard/digest"), "unavailable")
        self.assertEqual(self.status(report, "closure/native:" + BINARY + "/digest"), "match")
        self.assertIn("closure/bundle:standard/digest", report["refused"])

    def test_unavailable_interpreter_is_never_ok(self):
        manifest = self.save(self.build())
        code, report, _ = self.recompute(manifest, interpreter=False)
        self.assertEqual(code, 1)
        self.assertEqual(self.status(report, "closure/interpreter/digest"), "unavailable")
        self.assertEqual(self.status(report, "native/interpreter/sha256"), "unavailable")
        self.assertEqual(self.status(report, "digest"), "unavailable")
        self.assertFalse(report["ok"])

    def test_tampered_manifest_refused(self):
        record = json.loads(self.build())
        record["items"][".ai/tools/a.yaml"]["hash"] = "0" * 64
        code, _, result = self.recompute(self.save(json.dumps(record)))
        self.assertEqual(code, 2)
        self.assertIn("digest", result.stderr)

    def test_tree_with_symlink_is_refused_by_name(self):
        (self.bundle / ".ai/tools/link").symlink_to("a.yaml")
        result = self.run_tool(self.build_args())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("tools/link", result.stderr)
        self.assertIn("symbolic links", result.stderr)

    def test_unpaired_sidecar_and_binary_refused_by_name(self):
        (self.bin / "stray").write_bytes(b"x")
        (self.bin / "stray").chmod(0o644)
        result = self.run_tool(self.build_args())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("stray", result.stderr)
        (self.bin / "stray").unlink()
        (self.bin / "orphan.item_source.json").write_text("{}")
        result = self.run_tool(self.build_args())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("orphan.item_source.json", result.stderr)

    def test_extra_executable_and_missing_payload_refused(self):
        (self.bin / "extra").write_bytes(b"x")
        (self.bin / "extra").chmod(0o755)
        (self.bin / "extra.item_source.json").write_text("{}")
        (self.bin / "extra.item_source.json").chmod(0o644)
        result = self.run_tool(self.build_args())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("undeclared populated executable", result.stderr)
        self.assertIn("extra", result.stderr)
        (self.bin / "extra").unlink()
        (self.bin / "extra.item_source.json").unlink()
        (self.bin / BINARY).unlink()
        (self.bin / f"{BINARY}.item_source.json").unlink()
        result = self.run_tool(self.build_args())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("missing populated executable", result.stderr)

    def test_recompute_refuses_tree_that_breaks_audit_rules(self):
        manifest = self.save(self.build())
        (self.bundle / ".ai/tools/link").symlink_to("a.yaml")
        code, _, result = self.recompute(manifest)
        self.assertEqual(code, 2)
        self.assertIn("tools/link", result.stderr)

    def test_item_body_not_matching_envelope_refused(self):
        (self.bundle / ".ai/tools/a.yaml").write_text(
            signed("a: 1\n").replace("a: 1", "a: 9"))
        result = self.run_tool(self.build_args())
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(".ai/tools/a.yaml", result.stderr)

    def test_authority_class_is_plain_data(self):
        args = self.build_args()
        args[args.index("dev")] = "any-string-at-all"
        result = self.run_tool(args)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["authority"]["class"], "any-string-at-all")


if __name__ == "__main__":
    unittest.main()
