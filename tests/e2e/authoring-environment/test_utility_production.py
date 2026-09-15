"""Shared production-entry checks; synthetic inputs do not compile utilities."""
import io
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / ".ai/tools/ryeos/development/authoring-environment-production/lib"))
import utility_production as owner


class UtilityProductionTests(unittest.TestCase):
    def test_request_requires_existing_absolute_project_and_exact_configs(self):
        with tempfile.TemporaryDirectory() as directory:
            with patch.object(owner.sys, "argv", ["tool", "--project-path", directory]):
                with patch.object(owner.sys, "stdin", SimpleNamespace(buffer=io.BytesIO(b'{"resolved_config":{}}'))):
                    with self.assertRaisesRegex(ValueError, "exact utility production Configs"):
                        owner.parse_request()
                with patch.object(owner.sys, "stdin", SimpleNamespace(buffer=io.BytesIO(b"x" * 262145))):
                    with self.assertRaisesRegex(ValueError, "exceed bound"):
                        owner.parse_request()
            with patch.object(owner.sys, "argv", ["tool", "--project-path", "relative"]):
                with self.assertRaisesRegex(ValueError, "invalid admitted project"):
                    owner.parse_request()

    def test_failed_compiler_retains_bounded_log_not_private_cache(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            project, inputs = root / "project", root / "inputs"
            project.mkdir()
            (inputs / "bootstrap").mkdir(parents=True)
            archive = inputs / "bootstrap/sources.tar.gz"
            archive.write_bytes(b"synthetic selected container")
            expected = {"bootstrap/sources.tar.gz": {"bytes": archive.stat().st_size,
                                                      "sha256": owner.sha256(archive)}}
            resolved = {owner.INPUT_CONFIG: {}, owner.SOURCE_CONFIG: {"sources": [{"archive": "one.tar"}]},
                        owner.SUPPORT_CONFIG: {"support": {}}}
            work = root / "work"

            def compile_failure(sources, support, selected, scratch, destination):
                self.assertEqual((selected / "one.tar").read_bytes(), b"selected source bytes")
                self.assertEqual(scratch, work)
                (scratch / "logs").mkdir(parents=True)
                (scratch / "logs/coreutils.log").write_bytes(b"exact upstream failure")
                (scratch / "zig-cache").mkdir()
                raise ValueError("compiler refused")

            with patch.object(owner, "RAW_ROOT", inputs), patch.object(owner, "WORK", work), \
                    patch.object(owner, "selection", return_value=(expected, {}, None, "sources.tar.gz")), \
                    patch.object(owner, "read_members", return_value={"one.tar": b"selected source bytes"}), \
                    patch.object(owner, "build_utilities", side_effect=compile_failure), \
                    patch.object(owner, "Path", side_effect=lambda value: root / "selected"), \
                    patch.object(owner.sys, "stderr", io.StringIO()):
                with self.assertRaisesRegex(ValueError, "compiler refused"):
                    owner.produce(project, resolved)
            self.assertEqual((project / "products/authoring-build-logs/coreutils.log").read_bytes(),
                             b"exact upstream failure")
            self.assertFalse((project / "products/zig-cache").exists())


if __name__ == "__main__":
    unittest.main()
