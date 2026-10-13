"""Finalizer authoring/CLI contracts; installed admission remains separate.

The CLI receives fixtures with the exact strict resolver's projection shape.
Engine tests own signature verification and admitted snapshot selection; these
tests do not pretend a Python-created projection authenticates its own signer.
"""

import copy
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest import mock

import yaml


ROOT = Path(__file__).resolve().parents[3]
FINALIZER = ROOT / ".ai/tools/ryeos/development/cargo-vendor-finalize/run.py"
INTENT_PATH = "bundles/standard/.ai/config/development/ryeos/cargo-vendor-lock-intent.yaml"
INTENT = ROOT / INTENT_PATH
SIGNER = "a" * 64  # Synthetic already-verified projection, not a node credential.


def load_module(path, name):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    spec.loader.exec_module(module)
    return module


def tool_header():
    lines = []
    for line in FINALIZER.read_text().splitlines():
        if not line.startswith("#"):
            break
        if line.startswith("# ") and not line.startswith("# ryeos:signed:"):
            lines.append(line[2:])
    return yaml.safe_load("\n".join(lines))["ryeos-tool"]


def authored_request(lock):
    # Read and vary the supported Config body, never executable module state.
    intent = yaml.safe_load(INTENT.read_text())
    intent["expected_sha256"] = hashlib.sha256(lock).hexdigest()
    return {"resolved_config": {"value": intent, "source": {
        "bundle_name": "standard", "config_path": INTENT_PATH,
        "signer_fingerprint": SIGNER,
    }}}


def prepare_project(root, lock):
    (root / "Cargo.lock").write_bytes(lock)
    (root / "Cargo.toml").write_text('[package]\nname = "fixture"\nversion = "1.0.0"\n')
    (root / "products/cargo-vendor").mkdir(parents=True)


def run_finalizer(project, request):
    return subprocess.run(
        [sys.executable, "-I", "-B", str(FINALIZER), "--project-path", str(project)],
        input=json.dumps(request).encode(), stdout=subprocess.PIPE,
        stderr=subprocess.PIPE, env={"LANG": "C", "LC_ALL": "C", "PATH": ""},
        timeout=10, check=False,
    )


class CargoVendorFinalizerTests(unittest.TestCase):
    def setUp(self):
        self.module = load_module(FINALIZER, "cargo_vendor_finalizer")

    def test_signed_tool_selects_exact_source_config_and_refuses_caller_parameters(self):
        tool = tool_header()
        self.assertEqual(tool["config_resolve"], {
            "type": "strict_signed_project_bundle", "bundle_name": "standard",
            "config_path": INTENT_PATH,
        })
        self.assertEqual(tool["config_schema"], {
            "type": "object", "properties": {}, "additionalProperties": False,
        })
        self.assertEqual(tool["workspace_access"], "immutable_current_generation")
        self.assertEqual(tool["filesystem_authority"], "captured_execution")
        self.assertEqual(tool["network_authority"], "isolated")
        graph = yaml.safe_load((ROOT / ".ai/graphs/ryeos/development/cargo-vendor-production.yaml").read_text())
        for node in ("produce", "retain_lock"):
            self.assertEqual(graph["config"]["nodes"][node]["action"]["params"], {})
        self.assertEqual(graph["config"]["nodes"]["retain_lock"]["action"]["item_id"],
                         "tool:ryeos/development/cargo-vendor-finalize/run")
        self.assertNotIn("EXPECTED_SHA256", FINALIZER.read_text())
        self.assertEqual(self.module.admitted_lock_expectation({"resolved_config": {
            "value": yaml.safe_load(INTENT.read_text()), "source": {
                "bundle_name": "standard", "config_path": INTENT_PATH,
                "signer_fingerprint": SIGNER,
            },
        }}), yaml.safe_load(INTENT.read_text())["expected_sha256"])

    def test_two_source_lock_intents_use_unchanged_cli_and_preserve_exact_bytes(self):
        code_hash = hashlib.sha256(FINALIZER.read_bytes()).hexdigest()
        locks = [b'version = 4\n\n[[package]]\nname = "fixture"\nversion = "1.0.0"\n',
                 b'version = 4\n\n[[package]]\nname = "fixture"\nversion = "1.1.0"\n']
        observed = []
        for lock in locks:
            with self.subTest(lock=lock), tempfile.TemporaryDirectory() as temporary:
                project = Path(temporary)
                prepare_project(project, lock)
                before = (project / "Cargo.lock").stat()
                request = authored_request(lock)
                run = run_finalizer(project, request)
                self.assertEqual(run.returncode, 0, run.stderr.decode())
                result = json.loads(run.stdout)
                self.assertFalse(result["source_mutated"])
                self.assertEqual(result["bytes"], len(lock))
                self.assertEqual(result["sha256"], request["resolved_config"]["value"]["expected_sha256"])
                self.assertEqual((project / result["path"]).read_bytes(), lock)
                self.assertEqual((project / "Cargo.lock").read_bytes(), lock)
                after = (project / "Cargo.lock").stat()
                self.assertEqual((before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns),
                                 (after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns))
                observed.append(result["sha256"])
                duplicate = run_finalizer(project, request)
                self.assertNotEqual(duplicate.returncode, 0)
                self.assertIn(b"already contains", duplicate.stderr)
                self.assertEqual((project / result["path"]).read_bytes(), lock)
        self.assertNotEqual(*observed)
        self.assertEqual(hashlib.sha256(FINALIZER.read_bytes()).hexdigest(), code_hash)

    def test_wrong_source_or_stale_intent_refuses_before_output_publication(self):
        first, second = b'version = 4\n# generation A\n', b'version = 4\n# generation B\n'
        for source, intent in ((first, second), (second, first)):
            with self.subTest(source=source), tempfile.TemporaryDirectory() as temporary:
                project = Path(temporary)
                prepare_project(project, source)
                run = run_finalizer(project, authored_request(intent))
                self.assertNotEqual(run.returncode, 0)
                self.assertIn(b"Cargo.lock differs from the admitted release lock", run.stderr)
                self.assertFalse((project / "products/cargo-vendor/Cargo.lock").exists())
                self.assertEqual((project / "Cargo.lock").read_bytes(), source)

    def test_missing_ordinary_or_wrong_source_projection_refuses(self):
        correct = authored_request(b'version = 4\n')
        invalid = [{}, {"expected_sha256": correct["resolved_config"]["value"]["expected_sha256"]},
                   {"resolved_config": correct["resolved_config"]["value"]}]
        for key, value in (("bundle_name", "bundle-release"),
                           ("config_path", ".ai/config/development/ryeos/cargo-vendor-lock-intent.yaml"),
                           ("signer_fingerprint", "unsigned")):
            request = copy.deepcopy(correct)
            request["resolved_config"]["source"][key] = value
            invalid.append(request)
        for key, value in (("schema", "ryeos.cargo_vendor_lock_intent.v2"),
                           ("expected_sha256", "A" * 64), ("expected_sha256", None),
                           ("output", "Cargo.lock")):
            request = copy.deepcopy(correct)
            request["resolved_config"]["value"][key] = value
            invalid.append(request)
        for request in invalid:
            with self.subTest(request=request), self.assertRaises(ValueError):
                self.module.admitted_lock_expectation(request)

    def test_stable_read_rejects_identity_race(self):
        with tempfile.NamedTemporaryFile() as stream:
            stream.write(b'version = 4\n')
            stream.flush()
            real_fstat = os.fstat
            calls = 0

            def moved(descriptor):
                nonlocal calls
                value = real_fstat(descriptor)
                calls += 1
                if calls == 2:
                    fields = list(value)
                    fields[8] += 1
                    return os.stat_result(fields)
                return value

            with mock.patch.object(self.module.os, "fstat", side_effect=moved):
                with self.assertRaisesRegex(ValueError, "changed while retained"):
                    self.module.stable_lock(Path(stream.name))

    def test_source_symlink_and_output_ancestor_symlink_refuse(self):
        lock = b'version = 4\n'
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            project = root / "project"
            project.mkdir()
            prepare_project(project, lock)
            outside = root / "outside.lock"
            outside.write_bytes(lock)
            (project / "Cargo.lock").unlink()
            (project / "Cargo.lock").symlink_to(outside)
            with self.assertRaises(OSError):
                self.module.execute(authored_request(lock), project)
            (project / "Cargo.lock").unlink()
            (project / "Cargo.lock").write_bytes(lock)
            (project / "products").rename(root / "outside-products")
            (project / "products").symlink_to(root / "outside-products", target_is_directory=True)
            with self.assertRaisesRegex(ValueError, "ordinary directory"):
                self.module.execute(authored_request(lock), project)
            self.assertFalse((root / "outside-products/cargo-vendor/Cargo.lock").exists())

    def test_concurrent_output_is_never_overwritten(self):
        with tempfile.TemporaryDirectory() as temporary:
            destination = Path(temporary) / "Cargo.lock"
            real_link = os.link

            def concurrent_writer(source, target, **kwargs):
                target.write_bytes(b"other writer")
                return real_link(source, target, **kwargs)

            with mock.patch.object(self.module.os, "link", side_effect=concurrent_writer):
                with self.assertRaisesRegex(ValueError, "already contains"):
                    self.module.retain_lock(b"our retained lock", destination)
            self.assertEqual(destination.read_bytes(), b"other writer")
            self.assertEqual(list(destination.parent.iterdir()), [destination])

    def test_retained_locks_reach_same_vendor_checker_and_wrong_workspace_refuses(self):
        # Exercise the existing consuming algorithm with its Cargo subprocess
        # boundary stubbed. This establishes exact lock handoff, not an actual
        # offline Cargo run or daemon qualification publication.
        checker = load_module(ROOT / "bundles/standard/.ai/tools/ryeos/environments/qualification/cargo-vendor.py",
                              "cargo_vendor_checker")
        for version in ("1.0.0", "1.1.0"):
            lock = f'version = 4\n\n[[package]]\nname = "fixture"\nversion = "{version}"\n'.encode()
            with self.subTest(version=version), tempfile.TemporaryDirectory() as temporary:
                project = Path(temporary)
                prepare_project(project, lock)
                result = self.module.execute(authored_request(lock), project)
                metadata = {"packages": [{"name": "fixture", "version": version,
                                          "manifest_path": str(project / "Cargo.toml")}]}
                completed = subprocess.CompletedProcess([], 0, json.dumps(metadata).encode())
                with mock.patch.object(checker, "ROOT", project / "products/cargo-vendor"), \
                        mock.patch.object(checker.Path, "cwd", return_value=project), \
                        mock.patch.object(checker.subprocess, "run", return_value=completed) as cargo:
                    self.assertEqual(result["sha256"], hashlib.sha256(checker.bytes_stable(
                        checker.ROOT / "Cargo.lock")).hexdigest())
                    self.assertRegex(checker.probe_offline_cargo({}), r"^[0-9a-f]{64}$")
                    self.assertEqual(cargo.call_count, 1)
                    (project / "Cargo.lock").write_bytes(lock + b"# another source\n")
                    with self.assertRaisesRegex(ValueError, "workspace lock differs"):
                        checker.probe_offline_cargo({})
                    self.assertEqual(cargo.call_count, 1)


if __name__ == "__main__":
    unittest.main()
