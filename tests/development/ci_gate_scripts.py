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


class TouchedPackages(unittest.TestCase):
    """Which cargo packages a merge gate must test for a branch."""

    BASE_FILES = {
        "Cargo.toml": '[workspace]\nmembers = ["crates/a", "crates/bin/daemon"]\nexclude = ["crates/clients/rune"]\n',
        "crates/a/Cargo.toml": '[package]\nname = "pkg-a"\nversion = "0"\n',
        "crates/a/src/lib.rs": "pub fn f() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        assert!(true);\n    }\n}\n",
        "crates/a/src/other.rs": "pub fn g() -> u8 {\n    1\n}\n",
        "crates/bin/daemon/Cargo.toml": '[package]\nname = "ryeosd"\nversion = "0"\n',
        "crates/bin/daemon/src/main.rs": "fn main() {}\n",
        "crates/bin/daemon/tests/one_e2e.rs": "// e2e\n",
        "crates/clients/rune/Cargo.toml": '[workspace]\n\n[package]\nname = "rune-client"\nversion = "0"\n',
        "crates/clients/rune/src/lib.rs": "pub fn r() {}\n",
        "docs/readme.md": "x",
    }

    def packages(self, changed: dict[str, str], rule: str = "narrow") -> list[dict]:
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name)
        git(root, "init", "-q", "-b", "next")
        for name, text in self.BASE_FILES.items():
            write(root, name, text)
        git(root, "add", "-A")
        git(root, "commit", "-q", "-m", "base")
        git(root, "update-ref", "refs/remotes/origin/next", "HEAD")
        git(root, "checkout", "-q", "-b", "work")
        for name, text in changed.items():
            write(root, name, text)
        git(root, "add", "-A")
        git(root, "commit", "-q", "--allow-empty", "-m", "change")
        base = classify_mod.git(str(root), "merge-base", "HEAD", "origin/next").decode().strip()
        paths = sorted(
            n for n in classify_mod.git(str(root), "diff", "--name-only", "-z", base, "HEAD").decode().split("\0") if n
        )
        return classify_mod.touched_packages(str(root), base, paths, rule)

    def names(self, found: list[dict]) -> list[str]:
        return [entry["package"] for entry in found]

    def test_tests_dir_file_maps_to_its_crate(self):
        found = self.packages({"crates/a/tests/new_replay.rs": "#[test] fn t() {}\n"})
        self.assertEqual(found, [{"package": "pkg-a", "manifest": None, "args": []}])

    def test_cfg_test_hunk_maps_to_its_crate(self):
        lib = self.BASE_FILES["crates/a/src/lib.rs"].replace("assert!(true);", "assert!(1 == 1);")
        self.assertEqual(self.names(self.packages({"crates/a/src/lib.rs": lib})), ["pkg-a"])

    def test_added_test_function_in_test_module_maps(self):
        lib = self.BASE_FILES["crates/a/src/lib.rs"].replace(
            "    }\n}\n", "    }\n\n    #[test]\n    fn u() {}\n}\n"
        )
        self.assertEqual(self.names(self.packages({"crates/a/src/lib.rs": lib})), ["pkg-a"])

    def test_non_test_src_change_maps_to_none_under_narrow_rule(self):
        lib = self.BASE_FILES["crates/a/src/lib.rs"].replace("pub fn f() {}", "pub fn f() { let _ = 1; }")
        self.assertEqual(self.packages({"crates/a/src/lib.rs": lib, "crates/a/src/other.rs": "pub fn g() -> u8 { 2 }\n"}), [])

    def test_non_test_src_change_maps_under_wide_rule(self):
        found = self.packages({"crates/a/src/other.rs": "pub fn g() -> u8 { 2 }\n"}, rule="wide")
        self.assertEqual(self.names(found), ["pkg-a"])

    def test_rune_workspace_file_carries_its_manifest(self):
        found = self.packages({"crates/clients/rune/tests/snap.rs": "#[test] fn t() {}\n"})
        self.assertEqual(
            found,
            [{"package": "rune-client", "manifest": "crates/clients/rune/Cargo.toml", "args": []}],
        )

    def test_docs_only_change_maps_to_none(self):
        changed = {"docs/readme.md": "y", "crates/a/NOTES.md": "z"}
        self.assertEqual(self.packages(changed, rule="narrow"), [])
        self.assertEqual(self.packages(changed, rule="wide"), [])

    def test_daemon_e2e_is_limited(self):
        narrow = self.packages({"crates/bin/daemon/tests/one_e2e.rs": "// changed\n"})
        self.assertEqual(
            narrow[0]["args"], ["--test", "one_e2e", "--lib", "--bins", "--no-tests=pass"]
        )
        wide = self.packages({"crates/bin/daemon/src/main.rs": "fn main() { }\n"}, rule="wide")
        self.assertEqual(wide[0]["args"], ["--lib", "--bins", "--no-tests=pass"])

    def test_daemon_shared_test_helper_runs_whole_package(self):
        found = self.packages({"crates/bin/daemon/tests/common/mod.rs": "// helper\n"})
        self.assertEqual(found[0]["args"], [])

    def test_cfg_test_text_in_a_string_is_not_test_code(self):
        other = 'pub fn g() -> &\'static str {\n    "#[cfg(test)] mod tests { x }"\n}\n'
        self.assertEqual(self.packages({"crates/a/src/other.rs": other}), [])

    def test_cli_fault_prints_error_not_none(self):
        with tempfile.TemporaryDirectory() as tmp:
            done = subprocess.run(
                ["python3", "-I", str(CLASSIFY), "--root", tmp], capture_output=True, text=True
            )
        self.assertIn("test_packages=ERROR", done.stdout.splitlines())


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

    LOCK = "crates/clients/rune/Cargo.lock"

    def lock_text(self, path_version: str, registry_version: str = "1.0.0", edge: str = "serde") -> str:
        return (
            f'[[package]]\nname = "lillux"\nversion = "{path_version}"\ndependencies = [\n "{edge}",\n]\n\n'
            f'[[package]]\nname = "serde"\nversion = "{registry_version}"\n'
            'source = "registry+https://github.com/rust-lang/crates.io-index"\nchecksum = "abc"\n'
        )

    def test_path_crate_lock_version_is_masked_but_the_rest_of_the_lock_counts(self):
        root = self.repo()
        write(root, self.LOCK, self.lock_text("0.5.102"))
        git(root, "add", "-A")
        good = digest_mod.digest(root)
        write(root, self.LOCK, self.lock_text("0.6.0-ple.3"))
        self.assertEqual(digest_mod.digest(root), good)
        write(root, self.LOCK, self.lock_text("0.5.102", registry_version="1.0.1"))
        self.assertNotEqual(digest_mod.digest(root), good)
        write(root, self.LOCK, self.lock_text("0.5.102", edge="other"))
        self.assertNotEqual(digest_mod.digest(root), good)

    def test_a_path_crate_manifest_version_still_counts(self):
        root = self.repo()
        good = digest_mod.digest(root)
        write(root, "crates/clients/runtime/f.txt", 'version = "0.1.1"')
        self.assertNotEqual(digest_mod.digest(root), good)

    def stamp(self, root: Path, built_from: str) -> None:
        write(root, "crates/clients/rune/pkg/a.js", "package")
        pkg = Path(root) / "crates/clients/rune/pkg"
        digest_mod.write_stamp(Path(root), pkg, built_from)

    def test_stamp_from_the_current_sources_is_accepted(self):
        root = self.repo()
        self.stamp(root, digest_mod.digest(root))
        self.assertEqual(digest_mod.stamped_digest(root), digest_mod.digest(root))

    def test_missing_stamp_is_refused(self):
        root = self.repo()
        write(root, "crates/clients/rune/pkg/a.js", "package")
        with self.assertRaises(SystemExit):
            digest_mod.stamped_digest(root)

    def test_stamp_from_different_sources_is_refused(self):
        root = self.repo()
        self.stamp(root, digest_mod.digest(root))
        write(root, "crates/clients/base/f.txt", "changed after the build")
        with self.assertRaises(SystemExit):
            digest_mod.stamped_digest(root)
        self.stamp(root, "0" * 64)
        with self.assertRaises(SystemExit):
            digest_mod.stamped_digest(root)

    def test_package_changed_after_the_build_is_refused(self):
        root = self.repo()
        self.stamp(root, digest_mod.digest(root))
        write(root, "crates/clients/rune/pkg/a.js", "replaced by hand")
        with self.assertRaises(SystemExit):
            digest_mod.stamped_digest(root)

    def test_input_matching_nothing_fails(self):
        root = self.repo()
        git(root, "rm", "-q", "rust-toolchain.toml")
        with self.assertRaises(SystemExit):
            digest_mod.digest(root)


if __name__ == "__main__":
    unittest.main()
