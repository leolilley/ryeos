#!/usr/bin/env python3

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]
INSPECT = ROOT / "scripts/release/inspect-core-native-set.py"


def make_tree(base):
    bin_dir = base / "bundles/core/.ai/bin/x86_64-unknown-linux-gnu"
    bin_dir.mkdir(parents=True)
    for name in ("alpha", "beta"):
        (bin_dir / name).write_bytes(f"binary-{name}".encode())
        (bin_dir / f"{name}.item_source.json").write_text(f'{{"item_ref":"{name}"}}')
    ai = base / "bundles/core/.ai"
    (ai / "manifest.yaml").write_text("# ryeos:signed:1\nname: core\n")
    (ai / "manifest.source.yaml").write_text("name: core\n")
    (ai / "tools").mkdir()
    (ai / "tools/t.yaml").write_text("a: 1\n")
    (ai / "tools/t.py").write_text("print(1)\n")
    for label in ("daemon", "cli"):
        (base / label).write_bytes(label.encode())
    return base, bin_dir


class CoreNativeSetTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.base, self.bin = make_tree(Path(self._tmp.name) / "app")
        self.rec = Path(self._tmp.name) / "record.json"

    def run_tool(self, *extra):
        return subprocess.run(
            [sys.executable, "-I", str(INSPECT), "--root", str(self.base),
             "--binary", f"daemon={self.base / 'daemon'}",
             "--binary", f"cli={self.base / 'cli'}", *extra],
            text=True, capture_output=True, check=False)

    def record(self, *extra):
        result = self.run_tool(*extra)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.rec.write_text(result.stdout)
        return json.loads(result.stdout)

    def compare(self, *extra):
        return self.run_tool("--compare", str(self.rec), *extra)

    def test_identical_sets_compare_equal(self):
        record = self.record()
        self.assertEqual(len(record["digest"]), 64)
        self.assertEqual(self.run_tool().stdout, self.rec.read_text())
        result = self.compare()
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_changed_binary_reported(self):
        self.record()
        (self.bin / "alpha").write_bytes(b"other")
        result = self.compare()
        self.assertEqual(result.returncode, 1)
        self.assertIn("changed: bin/x86_64-unknown-linux-gnu/alpha", result.stdout)

    def test_changed_item_source_reported(self):
        self.record()
        (self.bin / "beta.item_source.json").write_text("{}")
        result = self.compare()
        self.assertEqual(result.returncode, 1)
        self.assertIn("item_sources/x86_64-unknown-linux-gnu/beta.item_source.json",
                      result.stdout)

    def test_added_and_removed_binary_reported(self):
        self.record()
        (self.bin / "gamma").write_bytes(b"g")
        (self.bin / "gamma.item_source.json").write_text("{}")
        (self.bin / "alpha").unlink()
        (self.bin / "alpha.item_source.json").unlink()
        result = self.compare()
        self.assertEqual(result.returncode, 1)
        self.assertIn("added: bin/x86_64-unknown-linux-gnu/gamma", result.stdout)
        self.assertIn("removed: bin/x86_64-unknown-linux-gnu/alpha", result.stdout)

    def test_selected_binary_change_reported(self):
        self.record()
        (self.base / "daemon").write_bytes(b"new")
        result = self.compare()
        self.assertEqual(result.returncode, 1)
        self.assertIn("selected_binaries/daemon/sha256", result.stdout)

    def test_changed_interpreter_reported(self):
        interp = Path(self._tmp.name) / "py"
        interp.write_text("#!/bin/sh\necho Python 3.0.1\n")
        interp.chmod(0o755)
        record = self.record("--interpreter", str(interp))
        self.assertEqual(record["interpreter"]["version"], "Python 3.0.1")
        self.assertEqual(self.compare("--interpreter", str(interp)).returncode, 0)
        interp.write_text("#!/bin/sh\necho Python 3.0.2\n")
        result = self.compare("--interpreter", str(interp))
        self.assertEqual(result.returncode, 1)
        self.assertIn("interpreter/sha256", result.stdout)
        self.assertIn("interpreter/version", result.stdout)
        self.assertEqual(self.compare().returncode, 1)

    def test_tool_files_outside_bin_do_not_change_digest(self):
        first = self.record()
        ai = self.base / "bundles/core/.ai"
        (ai / "tools/t.yaml").write_text("a: 2\n")
        (ai / "tools/t.py").write_text("print(2)\n")
        (ai / "tools/new.yaml").write_text("b: 1\n")
        self.assertEqual(self.compare().returncode, 0)
        self.assertEqual(json.loads(self.run_tool().stdout)["digest"], first["digest"])

    def test_manifest_resign_is_not_a_change_but_edit_is(self):
        self.record()
        manifest = self.base / "bundles/core/.ai/manifest.yaml"
        manifest.write_text("# ryeos:signed:2\nname: core\n")
        self.assertEqual(self.compare().returncode, 0)
        manifest.write_text("# ryeos:signed:2\nname: core2\n")
        self.assertEqual(self.compare().returncode, 1)

    def test_symlink_in_bin_refused_by_name(self):
        (self.bin / "link").symlink_to(self.base / "daemon")
        result = self.run_tool()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("link", result.stderr)

    def test_unpaired_files_refused_by_name(self):
        (self.bin / "stray").write_bytes(b"x")
        result = self.run_tool()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("stray", result.stderr)
        (self.bin / "stray").unlink()
        (self.bin / "orphan.item_source.json").write_text("{}")
        result = self.run_tool()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("orphan.item_source.json", result.stderr)

    def test_symlinked_bin_dir_refused(self):
        bin_root = self.base / "bundles/core/.ai/bin"
        moved = self.base / "elsewhere"
        bin_root.rename(moved)
        bin_root.symlink_to(moved)
        result = self.run_tool()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("symlink", result.stderr)

    def test_tampered_record_refused(self):
        record = self.record()
        record["bin"]["x86_64-unknown-linux-gnu/alpha"] = "0" * 64
        self.rec.write_text(json.dumps(record))
        self.assertEqual(self.compare().returncode, 2)

    def test_bundle_source_root_accepted(self):
        core = self.base / "bundles/core"
        result = subprocess.run(
            [sys.executable, "-I", str(INSPECT), "--root", str(core)],
            text=True, capture_output=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
