#!/usr/bin/env python3
"""Self-test for the merge-gate classifier and the web package digest check.

Builds throwaway git repositories; needs only git and python3.
"""

from __future__ import annotations

import importlib.util
import json
import subprocess
import tempfile
import unittest
from pathlib import Path

TOP = Path(__file__).resolve().parents[2]
DIGEST = TOP / ".ai/tools/ryeos/development/browser-assets-production/lib/web-package-source-digest.py"
CLASSIFY = TOP / ".ai/tools/ryeos/development/repository-validation/lib/classify-merge-gate.py"


def load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


digest_mod = load("digest_mod", DIGEST)
classify_mod = load("classify_mod", CLASSIFY)


def git(root: Path, *args: str) -> None:
    subprocess.run(
        ["git", "-c", "user.name=t", "-c", "user.email=t@t", "-C", str(root), *args],
        check=True,
        capture_output=True,
    )


def write(root: Path, name: str, text: str) -> None:
    path = root / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)


class Classifier(unittest.TestCase):
    def repo(self, changed: dict[str, str]) -> str:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        git(root, "init", "-q", "-b", "next")
        write(root, "README.md", "x")
        write(root, "crates/clients/rune/src/views.rs", "a")
        git(root, "add", "-A")
        git(root, "commit", "-q", "-m", "base")
        git(root, "update-ref", "refs/remotes/origin/next", "HEAD")
        git(root, "checkout", "-q", "-b", "work")
        for name, text in changed.items():
            write(root, name, text)
        git(root, "add", "-A")
        git(root, "commit", "-q", "--allow-empty", "-m", "change")
        return str(root)

    def run_classify(self, root: str) -> tuple[str, bool]:
        mode, _, _, bundles = classify_mod.classify(root, "origin/next")
        return mode, bundles

    def test_ui_only(self):
        root = self.repo(
            {
                "crates/clients/rune/src/views.rs": "b",
                "crates/daemon/ryeos-ui/src/lib.rs": "c",
                ".ai/knowledge/ryeos/development/ui-development.md": "d",
            }
        )
        self.assertEqual(self.run_classify(root), ("ui-only", False))

    def test_ui_bundle_marks_bundles_changed(self):
        root = self.repo({"bundles/ryeos-ui/x.yaml": "k"})
        self.assertEqual(self.run_classify(root), ("ui-only", True))

    def test_unknown_path_is_full(self):
        root = self.repo({"crates/clients/rune/src/views.rs": "b", "crates/engine/x.rs": "c"})
        self.assertEqual(self.run_classify(root)[0], "full")

    def test_other_bundle_is_full(self):
        root = self.repo({"bundles/other/x.yaml": "k"})
        self.assertEqual(self.run_classify(root), ("full", True))

    def test_ci_files_are_full(self):
        root = self.repo({"scripts/ci/x.sh": "k"})
        self.assertEqual(self.run_classify(root)[0], "full")

    def test_empty_diff_is_full(self):
        self.assertEqual(self.run_classify(self.repo({}))[0], "full")

    def test_missing_base_ref_is_full(self):
        root = self.repo({"crates/clients/rune/src/views.rs": "b"})
        self.assertEqual(classify_mod.classify(root, "origin/missing")[0], "full")

    def test_not_a_repository_is_full_via_cli(self):
        with tempfile.TemporaryDirectory() as tmp:
            done = subprocess.run(
                ["python3", "-I", str(CLASSIFY), "--root", tmp],
                capture_output=True,
                text=True,
            )
        self.assertEqual(done.returncode, 0)
        self.assertIn("mode=full", done.stdout.splitlines())

    def test_rename_out_of_ui_set_is_full(self):
        root = self.repo({})
        git(Path(root), "mv", "crates/clients/rune/src/views.rs", "elsewhere.rs")
        git(Path(root), "commit", "-q", "-m", "mv")
        self.assertEqual(self.run_classify(root)[0], "full")


class Digest(unittest.TestCase):
    def repo(self) -> Path:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        git(root, "init", "-q", "-b", "next")
        for entry in digest_mod.INPUTS:
            name = entry + "f.txt" if entry.endswith("/") else entry
            write(root, name, entry)
        write(root, "crates/clients/base/tests/t.rs", "t")
        write(root, "other/file", "o")
        git(root, "add", "-A")
        git(root, "commit", "-q", "-m", "base")
        return root

    def check(self, root: Path, recorded) -> int:
        manifest = {"schema_version": "v"}
        if recorded is not None:
            manifest["source_digest"] = recorded
        write(root, digest_mod.MANIFEST, json.dumps(manifest))
        return subprocess.run(
            ["python3", "-I", str(DIGEST), "--root", str(root), "--check"],
            capture_output=True,
        ).returncode

    def test_fresh_and_stale(self):
        root = self.repo()
        good = digest_mod.digest(root)
        self.assertEqual(self.check(root, good), 0)
        self.assertEqual(self.check(root, None), 1)
        self.assertEqual(self.check(root, "0" * 64), 1)
        write(root, "crates/clients/base/f.txt", "changed")
        self.assertNotEqual(digest_mod.digest(root), good)
        self.assertEqual(self.check(root, good), 1)

    def test_unrelated_and_test_paths_do_not_move_it(self):
        root = self.repo()
        good = digest_mod.digest(root)
        write(root, "other/file", "changed")
        write(root, "crates/clients/base/tests/t.rs", "changed")
        write(root, "crates/clients/rune/pkg/x.js", "changed")
        git(root, "add", "-A")
        self.assertEqual(digest_mod.digest(root), good)

    def test_untracked_files_do_not_count(self):
        root = self.repo()
        good = digest_mod.digest(root)
        write(root, "crates/clients/base/new.rs", "n")
        self.assertEqual(digest_mod.digest(root), good)

    def test_input_matching_nothing_fails(self):
        root = self.repo()
        git(root, "rm", "-q", "rust-toolchain.toml")
        with self.assertRaises(SystemExit):
            digest_mod.digest(root)


if __name__ == "__main__":
    unittest.main()
