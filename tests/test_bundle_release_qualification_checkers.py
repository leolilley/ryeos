"""Exercise the signed release checker algorithms against isolated product trees.

Only the fixed realization-root literal is redirected to a temporary directory.
The signed checker sources are read unchanged and never rewritten on disk.
The platform's signature, witness, and recipe admission remain separate tests.
"""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
CHECKERS = ROOT / "bundles/bundle-release/.ai/tools/ryeos/bundle-release/lib"
FIXED_ROOTS = {
    "portable": 'pathlib.Path("/ryeos/realizations/native-bundle")',
    "native": 'pathlib.Path("/ryeos/realizations/native-bundle")',
    "core-seed": 'pathlib.Path("/ryeos/realizations/core-seed")',
    "substrate": 'pathlib.Path("/ryeos/realizations/substrate-release")',
}


def run_checker(checker: str, tree: Path, parameters: dict | None = None) -> subprocess.CompletedProcess[str]:
    source = (CHECKERS / f"{checker}-qualify.py").read_text()
    fixed_root = FIXED_ROOTS[checker]
    if source.count(fixed_root) != 1:
        raise AssertionError("signed release checker changed its fixed realization ABI")
    source = source.replace(fixed_root, f"pathlib.Path({str(tree)!r})")
    env = os.environ.copy()
    env["RYEOS_EXTERNAL_REALIZATIONS"] = json.dumps([
        {"id": "python", "manifest_hash": "1" * 64},
        {"id": "subject", "manifest_hash": "2" * 64},
    ])
    env["RYE_THREAD_ID"] = "T-checker-fixture"
    return subprocess.run(
        [sys.executable, "-I", "-B", "-c", source],
        input=json.dumps(parameters if parameters is not None else {}),
        text=True,
        capture_output=True,
        env=env,
        check=False,
    )


def signed_bundle(tree: Path, name: str, *, executable: bool = False, signed: bool = True) -> None:
    (tree / ".ai").mkdir(parents=True)
    manifest = json.dumps({"name": name}, sort_keys=True, separators=(",", ":"))
    prefix = "# ryeos:signed:fixture\n" if signed else ""
    (tree / ".ai/manifest.yaml").write_text(prefix + manifest + "\n")
    if executable:
        binary = tree / ".ai/bin/fixture"
        binary.parent.mkdir()
        binary.write_bytes(b"fixture")
        binary.chmod(0o755)


class ReleaseQualificationCheckerTests(unittest.TestCase):
    def test_core_seed_keeps_its_fixed_identity_and_canonical_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            tree = Path(temporary) / "core"
            signed_bundle(tree, "core", executable=True)
            accepted = run_checker("core-seed", tree)
            self.assertEqual(accepted.returncode, 0, accepted.stderr)
            self.assertEqual(json.loads(accepted.stdout)["claims"], ["substrate_core_seed_checks_v1"])
            (tree / ".ai/manifest.yaml").write_text('# ryeos:signed:fixture\n{"name":"web"}\n')
            self.assertNotEqual(run_checker("core-seed", tree).returncode, 0)
            (tree / ".ai/manifest.yaml").write_text('# ryeos:signed:fixture\n{ "name": "core" }\n')
            self.assertNotEqual(run_checker("core-seed", tree).returncode, 0)

    def test_substrate_receipt_varies_target_but_rejects_open_or_noncanonical_data(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            tree = Path(temporary) / "substrate"
            receipt_path = tree / ".ai/substrate-release.json"
            receipt_path.parent.mkdir(parents=True)
            receipt = {
                "schema": "ryeos.substrate_build_receipt.v1",
                "kind": "substrate_build_receipt",
                "substrate_image_digest": "sha256:" + "a" * 64,
                "substrate_protocol": 1,
                "target": {"kind": "portable"},
                "core_generation_hash": "b" * 64,
            }

            def write(value: dict, *, pretty: bool = False) -> None:
                receipt_path.write_bytes(json.dumps(
                    value, sort_keys=True,
                    separators=None if pretty else (",", ":"),
                    ensure_ascii=False,
                ).encode())

            for target in ({"kind": "portable"}, {"kind": "triple", "triple": "x86_64-linux-gnu"}):
                receipt["target"] = target
                write(receipt)
                accepted = run_checker("substrate", tree)
                self.assertEqual(accepted.returncode, 0, accepted.stderr)
                self.assertEqual(json.loads(accepted.stdout)["probe_evidence"]["receipt"]["target"], target)
            write({**receipt, "unexpected": True})
            self.assertNotEqual(run_checker("substrate", tree).returncode, 0)
            write(receipt, pretty=True)
            self.assertNotEqual(run_checker("substrate", tree).returncode, 0)
            write(receipt)
            receipt_path.chmod(0o755)
            self.assertNotEqual(run_checker("substrate", tree).returncode, 0)

    def test_portable_accepts_two_non_core_names_with_one_signed_checker(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            for name in ("web", "admin"):
                tree = Path(temporary) / name
                signed_bundle(tree, name)
                completed = run_checker("portable", tree)
                self.assertEqual(completed.returncode, 0, completed.stderr)
                result = json.loads(completed.stdout)
                self.assertEqual(result["claims"], ["portable_bundle_release_checks_v1"])
                self.assertEqual(result["probe_evidence"]["bundle_name"], name)
                self.assertEqual(result["subject_manifest_hash"], "2" * 64)

    def test_native_accepts_two_non_core_names_with_one_signed_checker(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            for name in ("api", "worker"):
                tree = Path(temporary) / name
                signed_bundle(tree, name, executable=True)
                completed = run_checker("native", tree)
                self.assertEqual(completed.returncode, 0, completed.stderr)
                result = json.loads(completed.stdout)
                self.assertEqual(result["claims"], ["native_bundle_release_checks_v1"])
                self.assertEqual(result["probe_evidence"]["bundle_name"], name)
                self.assertEqual(result["probe_evidence"]["binary_count"], 1)

    def test_both_refuse_unsigned_source_core_and_unsupported_parameters(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            for checker in ("portable", "native"):
                for case, name, signed, parameters in (
                    ("unsigned", "web", False, None),
                    ("core", "core", True, None),
                    ("parameters", "web", True, {"unexpected": "value"}),
                ):
                    tree = Path(temporary) / checker / case
                    signed_bundle(tree, name, executable=checker == "native", signed=signed)
                    completed = run_checker(checker, tree, parameters)
                    self.assertNotEqual(completed.returncode, 0, (checker, case, completed.stdout))
                    self.assertEqual(completed.stdout, "")

    def test_portable_refuses_native_payload_and_native_requires_executable_payload(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            portable = Path(temporary) / "portable"
            signed_bundle(portable, "web", executable=True)
            self.assertNotEqual(run_checker("portable", portable).returncode, 0)
            native = Path(temporary) / "native"
            signed_bundle(native, "api")
            self.assertNotEqual(run_checker("native", native).returncode, 0)
            binary = native / ".ai/bin/fixture"
            binary.parent.mkdir()
            binary.write_bytes(b"fixture")
            binary.chmod(0o644)
            self.assertNotEqual(run_checker("native", native).returncode, 0)

    def test_both_refuse_links_in_captured_tree(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            for checker in ("portable", "native"):
                for link in ("symlink", "hardlink"):
                    tree = Path(temporary) / checker / link
                    signed_bundle(tree, "web", executable=checker == "native")
                    payload = tree / "payload"
                    payload.write_bytes(b"fixture")
                    linked = tree / "linked"
                    if link == "symlink":
                        linked.symlink_to(payload)
                    else:
                        os.link(payload, linked)
                    completed = run_checker(checker, tree)
                    self.assertNotEqual(completed.returncode, 0, (checker, link))


if __name__ == "__main__":
    unittest.main()
