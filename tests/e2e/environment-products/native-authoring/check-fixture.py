#!/usr/bin/env python3
"""Source-level native qualifier checks; no RyeOS admission or executable runs."""

import importlib.util
import hashlib
import json
from pathlib import Path
import tempfile
import sys
from unittest.mock import patch


# This checker imports authored source for inspection, not publication.
# Keep that source tree unchanged even when invoked without Python's -B flag.
sys.dont_write_bytecode = True

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[3]
TOOL = (ROOT /
    "bundles/standard/.ai/tools/ryeos/environments/qualification/native-authoring/verify.py")
SPEC = importlib.util.spec_from_file_location("native_authoring_verifier", TOOL)
verifier = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(verifier)


def realization(name, manifest, entries, size, mount):
    return {
        "id": name,
        "kind": "tree",
        "mode": "pinned",
        "manifest_hash": manifest,
        "entry_count": entries,
        "total_bytes": size,
        "mount_root": "execution_runtime",
        "mount": mount,
    }


def static_elf(_path, _support):
    return """ELF Header:
  Machine:                           Advanced Micro Devices X86-64
Program Headers:
  GNU_STACK      0x000000 0x000000 0x000000 0x000000 0x000000 RW  0x10
There is no dynamic section in this file.
"""


def write_runtime(root):
    for directory in ("bin", "lib", "licenses"):
        (root / directory).mkdir(parents=True)
    for command in verifier.REQUIRED_COMMANDS:
        path = root / "bin" / command
        path.write_bytes(b"\x7fELF" + command.encode())
        path.chmod(0o755)
    loader = root / "lib/ld-linux-x86-64.so.2"
    loader.write_bytes(b"\x7fELFfixture")
    loader.chmod(0o755)
    (root / "licenses/NOTICE").write_text("synthetic source test\n")


def main():
    source = TOOL.read_text()
    forbidden = ("config_resolve:", "local_binary", "production import", "/usr/bin/",
                 "expected_manifest_hash", "build-evidence.json", "provenance.json")
    if any(value in source for value in forbidden):
        raise SystemExit("native qualifier source gained an unauthenticated shortcut")
    if source.count("external_product_slots:") != 1 or source.count("#       relationship:") != 2:
        raise SystemExit("native qualifier Tool no longer has its exact two root slots")
    if "product_selections:" in source:
        raise SystemExit("native qualifier Tool must receive selectors only at launch")
    policy = (ROOT /
        "bundles/standard/.ai/config/ryeos/environments/qualification/native-authoring.yaml").read_text()
    if "allowed_claims: [authoring_runtime_closed]" not in policy:
        raise SystemExit("native qualifier policy lost its finite claim")
    qualification_runtime = (ROOT /
        "bundles/standard/.ai/tools/ryeos/environments/qualification/native-authoring/runtime.yaml").read_text()
    for required in (
        "executor_id: \"@subprocess\"",
        "execution_protocol: protocol:ryeos/core/opaque",
        "filesystem_authority: captured_execution",
        "network_authority: isolated",
        "command: realization:producer-python/lib/ld-musl-x86_64.so.1",
        "/ryeos/realizations/producer-python/python/bin/python3.14",
    ):
        if required not in qualification_runtime:
            raise SystemExit("trusted qualification runtime lost captured execution authority")
    if "config_resolve:" in qualification_runtime or "local_binary" in qualification_runtime:
        raise SystemExit("trusted qualification runtime gained a live Config or host interpreter")

    with tempfile.TemporaryDirectory(prefix="ryeos-native-authoring-source-check-") as directory:
        root = Path(directory) / "runtime"
        write_runtime(root)
        entries, _ = verifier.scan_runtime(root)
        size = sum(item.get("bytes", 0) for item in entries)
        values = [
            realization(verifier.SUBJECT_ID, "a" * 64, len(entries), size, "authoring-tools"),
            realization(verifier.SUPPORT_ID, "b" * 64, 1, 1, "authoring-inputs"),
            realization(verifier.PYTHON_ID, verifier.PYTHON_MANIFEST, 1, 1, "producer-python"),
        ]
        with patch.object(verifier, "verify_inspector", return_value="c" * 64), patch.object(
                verifier, "_readelf", static_elf), patch.object(
                verifier, "exercise_runtime", return_value={"checked": True}):
            result = verifier.qualify(json.dumps(values), root, Path(directory) / "support")
        assert result["subject_manifest_hash"] == "a" * 64
        assert result["claims"] == ["authoring_runtime_closed"]
        evidence = result["probe_evidence"]
        assert set(evidence) == {
            "schema", "command_contract_digest", "command_count", "elf_closure_digest",
            "elf_count", "inspector_identity_digest", "network_contacted",
            "runtime_inventory_digest", "runtime_probe_digest",
        }
        assert evidence["command_count"] == len(verifier.REQUIRED_COMMANDS)
        assert evidence["elf_count"] == len(verifier.REQUIRED_COMMANDS) + 1
        assert evidence["inspector_identity_digest"] == "c" * 64
        assert not evidence["network_contacted"]
        assert len(json.dumps(evidence, separators=(",", ":"))) < 2048

        changed = list(values)
        changed[0] = dict(changed[0], total_bytes=size + 1)
        with patch.object(verifier, "verify_inspector", return_value="c" * 64), patch.object(
                verifier, "_readelf", static_elf), patch.object(
                verifier, "exercise_runtime", return_value={"checked": True}):
            try:
                verifier.qualify(json.dumps(changed), root, Path(directory) / "support")
            except ValueError as error:
                assert "metrics contradict" in str(error)
            else:
                raise AssertionError("contradictory sealed metrics were accepted")

        (root / "bin/sed").unlink()
        with patch.object(verifier, "verify_inspector", return_value="c" * 64), patch.object(
                verifier, "_readelf", static_elf), patch.object(
                verifier, "exercise_runtime", return_value={"checked": True}):
            try:
                verifier.qualify(json.dumps(values), root, Path(directory) / "support")
            except ValueError as error:
                assert "metrics contradict" in str(error) or "command contract" in str(error)
            else:
                raise AssertionError("incomplete selected command set was accepted")

        support = Path(directory) / "inspector"
        member = support / "elf/bin/readelf"
        member.parent.mkdir(parents=True)
        member.write_bytes(b"exact inspector")
        member.chmod(0o755)
        identity = (member.stat().st_size, 0o755, hashlib.sha256(member.read_bytes()).hexdigest())
        with patch.dict(verifier.INSPECTOR_FILES, {"elf/bin/readelf": identity}, clear=True):
            assert len(verifier.verify_inspector(support)) == 64
            member.write_bytes(b"changed inspector")
            try:
                verifier.verify_inspector(support)
            except ValueError as error:
                assert "wrong" in str(error)
            else:
                raise AssertionError("changed selected inspector was accepted")

    print("native authoring qualifier source, bounds, identities and compact result checked")
    print("No RyeOS admission, signing, node, product capture or qualification was run")


if __name__ == "__main__":
    main()
