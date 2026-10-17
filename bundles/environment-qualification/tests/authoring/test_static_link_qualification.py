# ryeos:signed:2026-10-16T09:33:55Z:8a0d2cd99b78b450f69e4a7f4630e54addb9953a246f3ae1f374f1052e81e02e:FkQHTjiY58YHPLfJKeQ1VMhCIsK+iqi3nwPDL6zql7yLLvWKWnUMZiIbYJPWSiLqYN6cE5+/fmKInao2HX+DAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Independent static input closure verification, including fail-closed metadata."""
import hashlib
import copy
import importlib.util
import json
import os
import sys
from pathlib import Path
import tempfile
import unittest
from unittest import mock
import yaml

sys.dont_write_bytecode = True
REPO = Path(__file__).resolve().parents[4]
TOOL = REPO / "bundles/environment-qualification/.ai/tools/ryeos/environments/qualification/static-link-inputs.py"
SPEC = importlib.util.spec_from_file_location("static_qualification", TOOL)
Q = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(Q)
POLICY = yaml.safe_load((REPO / "bundles/environment-qualification/.ai/config/ryeos/environments/qualification/static-link-inputs.yaml").read_text())["product_qualification_policy"]
PROFILE = POLICY["verifier_parameters"]
FILES = Q.expected_files(PROFILE)
DIRECTORIES = {str(parent) for name in FILES for parent in Path(name).parents if str(parent) != "."}
PRODUCER = "800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf"


class StaticQualificationTests(unittest.TestCase):
    def identities(self):
        def identity(id, digest, mount, count, size):
            return dict(id=id, kind="tree", mode="pinned", manifest_hash=digest,
                        entry_count=count, total_bytes=size, mount_root="execution_runtime", mount=mount)
        return [identity("producer-python", PRODUCER, "producer-python", 4480, 68732630),
                identity("subject", "a" * 64, "static-link-inputs", len(FILES) + len(DIRECTORIES),
                         sum(size for size, _ in FILES.values()))]

    def test_independent_pins_match_signed_input_contract(self):
        config = REPO / ".ai/config/development/ryeos/static-link-inputs.yaml"
        data = json.loads("\n".join(line for line in config.read_text().splitlines() if not line.startswith("#")))
        self.assertEqual(FILES, {name: (entry["bytes"], entry["sha256"]) for name, entry in data["inputs"].items()})
        self.assertEqual(sum(size for size, _ in FILES.values()), 9989612)
        self.assertEqual(len(FILES), 11)
        header = []
        for line in TOOL.read_text().splitlines():
            if not line.startswith("#"):
                break
            if line.startswith("# ryeos:signed:"):
                continue
            if line.startswith("# "):
                header.append(line[2:])
        tool = yaml.safe_load("\n".join(header))["ryeos-tool"]
        self.assertEqual(tool["external_content"][0]["digest"], PRODUCER)
        self.assertEqual(tool["external_content"][0]["id"], "producer-python")

    def test_sealed_identity_and_bounds_reject_changes(self):
        for field, value in [("mount", "platform"), ("entry_count", True), ("total_bytes", 1),
                             ("manifest_hash", PRODUCER), ("mode", "live"), ("extra", 1)]:
            identities = self.identities()
            identities[1][field] = value
            with mock.patch.dict(os.environ, RYEOS_EXTERNAL_REALIZATIONS=json.dumps(identities)):
                with self.assertRaises(ValueError):
                    Q.realizations(FILES)

    def test_output_claims_closure_not_execution(self):
        with mock.patch.dict(os.environ, RYEOS_EXTERNAL_REALIZATIONS=json.dumps(self.identities())), \
                mock.patch.object(Q, "inventory"):
            result = Q.execute(PROFILE)
        self.assertEqual(result["claims"], ["static_link_inputs_profile_x86_64_linux_gnu_v2"])
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
                Q.inventory(root, FILES)
            (root / "unexpected").mkdir()
            with self.assertRaises(ValueError):
                Q.inventory(root, FILES)
            (root / "unexpected").rmdir()
            (root / "usr").symlink_to(root, target_is_directory=True)
            with self.assertRaises(ValueError):
                Q.inventory(root, FILES)

    def test_exact_inventory_succeeds(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            files = {"usr/lib/test.a": (4, hashlib.sha256(b"test").hexdigest())}
            (root / "usr/lib").mkdir(parents=True)
            (root / "usr/lib/test.a").write_bytes(b"test")
            (root / "usr/lib/test.a").chmod(0o644)
            for directory in (root / "usr", root / "usr/lib"):
                directory.chmod(0o755)
            Q.inventory(root, files)
            (root / "extra").write_bytes(b"")
            with self.assertRaises(ValueError):
                Q.inventory(root, files)

    def test_two_admitted_closures_use_one_checker_and_reject_swaps(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            profile_a = copy.deepcopy(PROFILE)
            profile_b = copy.deepcopy(PROFILE)
            contents = {name: name.encode() for name in PROFILE["files"]}
            changed = "usr/lib/x86_64-linux-gnu/libm.a"
            for name, data in contents.items():
                destination = root / name
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_bytes(data)
                destination.chmod(0o644)
                for profile in (profile_a, profile_b):
                    profile["files"][name] = {"bytes": len(data),
                                              "sha256": hashlib.sha256(data).hexdigest()}
            for directory in root.rglob("*"):
                if directory.is_dir():
                    directory.chmod(0o755)
            replacement = b"X" * len(contents[changed])
            profile_b["files"][changed]["sha256"] = hashlib.sha256(replacement).hexdigest()
            self.assertNotEqual(profile_a, profile_b)

            def run(profile, subject):
                identity = self.identities()
                identity[1]["manifest_hash"] = subject
                identity[1]["total_bytes"] = sum(len(data) for data in contents.values())
                with mock.patch.object(Q, "ROOT", root), mock.patch.dict(
                        os.environ, RYEOS_EXTERNAL_REALIZATIONS=json.dumps(identity)):
                    return Q.execute(profile)

            accepted_a = run(profile_a, "a" * 64)
            self.assertEqual(accepted_a["claims"], ["static_link_inputs_profile_x86_64_linux_gnu_v2"])
            with self.assertRaisesRegex(ValueError, "checksum differs"):
                run(profile_b, "a" * 64)
            (root / changed).write_bytes(replacement)
            accepted_b = run(profile_b, "b" * 64)
            self.assertEqual(accepted_b["claims"], accepted_a["claims"])
            self.assertNotEqual(accepted_b["probe_evidence"]["closure_digest"],
                                accepted_a["probe_evidence"]["closure_digest"])
            with self.assertRaisesRegex(ValueError, "checksum differs"):
                run(profile_a, "b" * 64)
            with self.assertRaisesRegex(ValueError, "subject identity changed"):
                run(profile_b, PRODUCER)

    def test_profile_rejects_missing_or_unsupported_expectations(self):
        for change in (
                lambda value: value.update(extra=True),
                lambda value: value.update(files={}),
                lambda value: value["files"]["usr/lib/x86_64-linux-gnu/libm.a"].update(bytes=True),
                lambda value: value.update(target="aarch64-unknown-linux-gnu"),
                lambda value: value.update(schema="ryeos.static_link_input_profile.v1"),
        ):
            profile = copy.deepcopy(PROFILE)
            change(profile)
            with self.assertRaises(ValueError):
                Q.expected_files(profile)

    def test_changed_versioned_member_uses_same_checker_and_refuses_crossed_profile(self):
        old = "usr/lib/x86_64-linux-gnu/libm-2.41.a"
        new = "usr/lib/x86_64-linux-gnu/libm-2.42.a"
        linker_script = "usr/lib/x86_64-linux-gnu/libm.a"
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)

            def install(versioned):
                contents = {name: name.encode() for name in PROFILE["files"] if name != old}
                contents[versioned] = b"synthetic archive"
                contents[linker_script] = f"GROUP ( {Path(versioned).name} libmvec.a )\n".encode()
                for stale in (old, new):
                    (root / stale).unlink(missing_ok=True)
                profile = copy.deepcopy(PROFILE)
                profile["files"] = {}
                for name, data in contents.items():
                    destination = root / name
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    destination.write_bytes(data)
                    destination.chmod(0o644)
                    profile["files"][name] = {"bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
                for directory in root.rglob("*"):
                    if directory.is_dir():
                        directory.chmod(0o755)
                return profile

            def run(profile, subject):
                identities = self.identities()
                identities[1]["manifest_hash"] = subject
                identities[1]["total_bytes"] = sum(path.stat().st_size for path in root.rglob("*") if path.is_file())
                with mock.patch.object(Q, "ROOT", root), mock.patch.dict(
                        os.environ, RYEOS_EXTERNAL_REALIZATIONS=json.dumps(identities)):
                    return Q.execute(profile)

            profile_a = install(old)
            result_a = run(profile_a, "a" * 64)
            profile_b = install(new)
            result_b = run(profile_b, "b" * 64)
            self.assertEqual(result_a["claims"], result_b["claims"])
            self.assertNotEqual(result_a["probe_evidence"]["closure_digest"],
                                result_b["probe_evidence"]["closure_digest"])
            with self.assertRaises(ValueError):
                run(profile_a, "b" * 64)
            install(old)
            with self.assertRaises(ValueError):
                run(profile_b, "a" * 64)
            self.assertNotIn("static_link_inputs_x86_64_linux_gnu_v1", result_a["claims"])

    def test_member_profile_rejects_unsafe_paths_and_entry_bound_overflow(self):
        expectation = {"bytes": 1, "sha256": "a" * 64}
        for name in (
                "/usr/lib/x86_64-linux-gnu/libm.a", "usr/lib/x86_64-linux-gnu/../libm.a",
                "usr/lib/x86_64-linux-gnu/nested/libm.a", "usr/lib/aarch64-linux-gnu/libm.a",
                "usr/lib/x86_64-linux-gnu/libm.so", "usr/lib/x86_64-linux-gnu/libm.a\n",
                "usr/lib/x86_64-linux-gnu/" + "x" * 256 + ".a", 1,
        ):
            profile = {**PROFILE, "files": {name: expectation}}
            with self.subTest(name=name), self.assertRaises(ValueError):
                Q.expected_files(profile)
        profile = {**PROFILE, "files": {
            f"usr/lib/x86_64-linux-gnu/lib{number}.a": expectation for number in range(62)}}
        with self.assertRaisesRegex(ValueError, "product bounds"):
            Q.expected_files(profile)

    def test_current_policy_is_required_by_development_and_release_consumers(self):
        expected = {"policy_ref": "config:ryeos/environments/qualification/static-link-inputs",
                    "required_claims": ["static_link_inputs_profile_x86_64_linux_gnu_v2"]}
        paths = (
            REPO / ".ai/config/development/ryeos/static-link-input-products.yaml",
            REPO / "bundles/bundle-release/.ai/config/bundle-release/execution-tool-products.yaml",
            REPO / "bundles/bundle-release/.ai/config/bundle-release/execution-environment-products.yaml",
        )
        found = set()
        for path in paths:
            relationships = yaml.safe_load(path.read_text())["product_relationships"]["relationships"]
            for relation in relationships:
                if relation["name"] in {
                    "static_link_inputs_to_native_build", "static_link_inputs_to_core_seed_build",
                    "static_link_inputs_to_native_build_tool", "static_link_inputs_to_core_seed_build_tool",
                }:
                    self.assertEqual(relation["qualification"], expected)
                    found.add(relation["name"])
        self.assertEqual(found, {
            "static_link_inputs_to_native_build", "static_link_inputs_to_core_seed_build",
            "static_link_inputs_to_native_build_tool", "static_link_inputs_to_core_seed_build_tool",
        })


if __name__ == "__main__":
    unittest.main()
