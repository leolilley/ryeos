# ryeos:signed:2026-09-22T10:08:57Z:99073f86769253cfa4060754fabca32725f73ffec0766f3652df4c0adbb509e5:Kiw47U2KjGdtLWZXDJoUvRW1cl4uZGkwc2F3dvOOa35lWRfGznJeX4Shq5QnLLPy/rSaYOp9X/xm9McpRrgxDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Regression coverage for qualifying the admitted workspace's vendor closure."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

sys.dont_write_bytecode = True

TOOL = Path(__file__).resolve().parents[4] / "bundles/standard/.ai/tools/ryeos/environments/qualification/cargo-vendor.py"
SPEC = importlib.util.spec_from_file_location("vendor_workspace_qualification", TOOL)
VENDOR = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(VENDOR)
SOURCE = "registry+https://github.com/rust-lang/crates.io-index"


class VendorWorkspaceQualificationTests(unittest.TestCase):
    def test_release_authority_preserves_nested_vendor_target_sources(self):
        profile = TOOL.parents[7] / "bundles/.ai/node/init/profiles/release-authority.yaml"
        text = profile.read_text()
        self.assertIn('      - "/target/"', text)
        self.assertNotIn('      - "target/"', text)
        pattern_block = text.split("  ingest_ignore:\n", 1)[1].split("  external_content:\n", 1)[0]
        patterns = [json.loads(line.strip().removeprefix("- "))
                    for line in pattern_block.splitlines() if line.startswith("      - ")]
        self.assertEqual(patterns, sorted(set(patterns)))
        closure_block = text.split("  object_closure:\n", 1)[1].split("  thread_history:\n", 1)[0]
        def limit(name):
            return int(closure_block.split(f"    {name}: ", 1)[1].splitlines()[0])
        blob_limit = limit("max_total_blob_bytes")
        # The independently verified vendor closure measured 671,156,525 bytes.
        self.assertGreaterEqual(blob_limit, 671_156_525)
        encoded_blobs = ((blob_limit + 2) // 3) * 4
        minimum_response = (4096 + limit("max_total_object_bytes") + encoded_blobs
                            + (limit("max_objects") + limit("max_blobs")) * 256)
        self.assertLessEqual(minimum_response, limit("max_response_bytes"))
        self.assertLessEqual(limit("max_response_bytes"), 1024 * 1024 * 1024)
        # The retained verifier jointly names Python, vendor and platform CAS
        # content. Its local aggregate is distinct from the wire response.
        local = closure_block.split("    local_verification:\n", 1)[1]
        local_limit = lambda name: int(local.split(f"      {name}: ", 1)[1].splitlines()[0])
        self.assertGreaterEqual(local_limit("max_blobs"), 40_537)
        self.assertGreaterEqual(local_limit("max_total_blob_bytes"), 1_007_471_595)
        self.assertLessEqual(local_limit("max_total_blob_bytes"), 1024 * 1024 * 1024)

    def fixture(self, root):
        project, subject = root / "project", root / "vendor"
        project.mkdir()
        subject.mkdir()
        for directory in (project, subject):
            (directory / "Cargo.lock").write_text("version = 4\n")
        (project / "Cargo.toml").write_text('[workspace]\nmembers = []\n')
        return project, subject

    def invoke(self, project, subject, packages, callback=None):
        def run(command, **kwargs):
            if callback:
                callback(command, kwargs)
            return subprocess.CompletedProcess(command, 0, json.dumps({"packages": packages}).encode())
        with mock.patch.object(Path, "cwd", return_value=project), \
                mock.patch.object(VENDOR, "ROOT", subject), \
                mock.patch.object(VENDOR.subprocess, "run", side_effect=run):
            return VENDOR.probe_offline_cargo({("reqwest", "0.12.28"): "a" * 64,
                                             ("optional", "1.0.0"): "b" * 64})

    def test_real_workspace_locked_resolution_and_private_state(self):
        with tempfile.TemporaryDirectory() as directory:
            project, subject = self.fixture(Path(directory))
            def inspect(command, kwargs):
                self.assertEqual(kwargs["cwd"], project)
                self.assertEqual(command[command.index("--manifest-path") + 1], str(project / "Cargo.toml"))
                for flag in ("--locked", "--frozen", "--offline"):
                    self.assertIn(flag, command)
                environment = kwargs["env"]
                roots = [Path(environment[key]) for key in ("HOME", "CARGO_HOME", "CARGO_TARGET_DIR", "TMPDIR")]
                self.assertEqual(len(set(roots)), 4)
                self.assertTrue(all(path.is_dir() and not list(path.iterdir()) for path in roots))
                self.assertEqual(environment["PATH"], "")
                self.assertNotIn("RUSTUP_HOME", environment)
            package = {"name": "reqwest", "version": "0.12.28", "source": SOURCE,
                       "manifest_path": "/some/vendor/reqwest/Cargo.toml"}
            first = self.invoke(project, subject, [package], inspect)
            second = self.invoke(project, subject, [{**package, "manifest_path": "/different/root/Cargo.toml"}])
            self.assertEqual(first, second)

    def test_rejects_different_project_lock_before_cargo(self):
        with tempfile.TemporaryDirectory() as directory:
            project, subject = self.fixture(Path(directory))
            (project / "Cargo.lock").write_text("different")
            with self.assertRaisesRegex(ValueError, "workspace lock differs"):
                self.invoke(project, subject, [])

    def test_rejects_source_config_and_outside_lock_resolution(self):
        with tempfile.TemporaryDirectory() as directory:
            project, subject = self.fixture(Path(directory))
            (project / ".cargo").mkdir()
            config = project / ".cargo/config.toml"
            config.write_text('[source.crates-io]\nreplace-with="ambient"\n')
            with self.assertRaisesRegex(ValueError, "configuration is not admitted"):
                self.invoke(project, subject, [])
            config.unlink()
            with self.assertRaisesRegex(ValueError, "outside the retained lock"):
                self.invoke(project, subject, [{"name": "native-tls", "version": "0.2.0", "source": SOURCE}])

    def test_rejects_path_dependency_outside_admitted_generation(self):
        with tempfile.TemporaryDirectory() as directory:
            project, subject = self.fixture(Path(directory))
            escaped = Path(directory) / "Cargo.toml"
            escaped.write_text("")
            with self.assertRaisesRegex(ValueError, "escaped admitted workspace"):
                self.invoke(project, subject, [{"name": "escape", "source": None, "manifest_path": str(escaped)}])


if __name__ == "__main__":
    unittest.main()
