# ryeos:signed:2026-09-22T05:04:44Z:c80331a9a4e2d18b00e72008cfd95b3fe1b9c615ec9d3a001f66d555067a977d:nrxWCLQJUKnP8Q+rzjG8BtsGQhnxjQuAskgQhf1xE3nU9XqQk2rnaG8iHA3o2B2eAMQEaG9VNjJk6UJp3WqrDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Independent static input closure verification, including fail-closed metadata."""
import hashlib
import importlib.util
import json
import os
import sys
from pathlib import Path
import tempfile
import unittest
from unittest import mock

sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parents[4]
TOOL = REPO / "bundles/standard/.ai/tools/ryeos/environments/qualification/static-link-inputs.py"
SPEC = importlib.util.spec_from_file_location("static_qualification", TOOL)
Q = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(Q)


class StaticQualificationTests(unittest.TestCase):
    def identities(self):
        def identity(id, digest, mount, count, size):
            return dict(id=id, kind="tree", mode="pinned", manifest_hash=digest,
                        entry_count=count, total_bytes=size, mount_root="execution_runtime", mount=mount)
        return [identity("producer-python", Q.PRODUCER, "producer-python", 4480, 68732630),
                identity("subject", "a" * 64, "static-link-inputs", len(Q.FILES) + len(Q.DIRECTORIES),
                         sum(size for size, _ in Q.FILES.values()))]

    def test_independent_pins_match_signed_input_contract(self):
        config = REPO / ".ai/config/development/ryeos/static-link-inputs.yaml"
        data = json.loads("\n".join(line for line in config.read_text().splitlines() if not line.startswith("#")))
        self.assertEqual(Q.FILES, {name: (entry["bytes"], entry["sha256"]) for name, entry in data["inputs"].items()})
        self.assertEqual(sum(size for size, _ in Q.FILES.values()), 9989612)
        self.assertEqual(len(Q.FILES), 11)

    def test_sealed_identity_and_bounds_reject_changes(self):
        for field, value in [("mount", "platform"), ("entry_count", True), ("total_bytes", 1),
                             ("manifest_hash", Q.PRODUCER), ("mode", "live"), ("extra", 1)]:
            identities = self.identities()
            identities[1][field] = value
            with mock.patch.dict(os.environ, RYEOS_EXTERNAL_REALIZATIONS=json.dumps(identities)):
                with self.assertRaises(ValueError):
                    Q.realizations()

    def test_output_claims_closure_not_execution(self):
        with mock.patch.dict(os.environ, RYEOS_EXTERNAL_REALIZATIONS=json.dumps(self.identities())), \
                mock.patch.object(Q, "inventory"):
            result = Q.execute({})
        self.assertEqual(result["claims"], ["static_link_inputs_x86_64_linux_gnu_v1"])
        self.assertFalse(result["probe_evidence"]["compiler_execution_proven"])
        self.assertEqual(result["subject_manifest_hash"], "a" * 64)

    def test_file_metadata_and_checksum(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            file = root / "file"
            file.write_bytes(b"test")
            file.chmod(0o644)
            digest = hashlib.sha256(b"test").hexdigest()
            Q.verify_file(file, 4, digest)
            with self.assertRaises(ValueError):
                Q.verify_file(file, 4, "a" * 64)
            with self.assertRaises(ValueError):
                Q.verify_file(file, 3, digest)
            file.chmod(0o755)
            with self.assertRaises(ValueError):
                Q.verify_file(file, 4, digest)
            file.chmod(0o644)
            os.link(file, root / "hard")
            with self.assertRaises(ValueError):
                Q.verify_file(file, 4, digest)
            (root / "soft").symlink_to(file)
            with self.assertRaises(OSError):
                Q.verify_file(root / "soft", 4, digest)

    def test_inventory_rejects_missing_extra_and_symlink_directories(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with self.assertRaises(ValueError):
                Q.inventory(root)
            (root / "unexpected").mkdir()
            with self.assertRaises(ValueError):
                Q.inventory(root)
            (root / "unexpected").rmdir()
            (root / "usr").symlink_to(root, target_is_directory=True)
            with self.assertRaises(ValueError):
                Q.inventory(root)

    def test_exact_inventory_succeeds(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            files = {"usr/lib/test.a": (4, hashlib.sha256(b"test").hexdigest())}
            (root / "usr/lib").mkdir(parents=True)
            (root / "usr/lib/test.a").write_bytes(b"test")
            (root / "usr/lib/test.a").chmod(0o644)
            for directory in (root / "usr", root / "usr/lib"):
                directory.chmod(0o755)
            with mock.patch.object(Q, "FILES", files), mock.patch.object(Q, "DIRECTORIES", {"usr", "usr/lib"}):
                Q.inventory(root)
                (root / "extra").write_bytes(b"")
                with self.assertRaises(ValueError):
                    Q.inventory(root)


if __name__ == "__main__":
    unittest.main()
