# ryeos:signed:2026-10-16T09:32:15Z:a9bddbf3cb46b304b0433117ac370630df9bc9a895295152dca7e17bb15c3f71:4KeKNMJ1PSGoxcxwqHTaMck4n/ZUOKl6KaPDTMka4YKIYenXBNppEN8m7UBV2EcQlZoZQLOWafFPfsWLCflXCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/cargo-vendor-finalize
#   version: "2.0.0"
#   description: Retain the exact admitted Cargo.lock witness in the Cargo vendor product
#   executor_id: tool:ryeos/development/authoring-environment-production/runtime
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   workspace_access: immutable_current_generation
#   filesystem_authority: captured_execution
#   network_authority: isolated
#   config_schema:
#     type: object
#     properties: {}
#     additionalProperties: false
#   config_resolve:
#     type: strict_signed_project_bundle
#     bundle_name: environment-qualification
#     config_path: bundles/environment-qualification/.ai/config/development/ryeos/cargo-vendor-lock-intent.yaml
#   external_content:
#     - id: producer-python
#       kind: tree
#       mode: pinned
#       digest: 800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf
#       mount_root: execution_runtime
#       mount: producer-python
"""Finalize a vendor tree with its exact immutable-generation lock witness."""

import hashlib
import json
import os
from pathlib import Path
import re
import stat
import sys
import tempfile


LOCK = Path("Cargo.lock")
OUTPUT = Path("products/cargo-vendor/Cargo.lock")
INTENT_CONFIG = "bundles/environment-qualification/.ai/config/development/ryeos/cargo-vendor-lock-intent.yaml"
HASH = re.compile(r"[0-9a-f]{64}")
MAX_LOCK_BYTES = 4 * 1024 * 1024


def admitted_lock_expectation(params):
    """Read the engine's sealed Config projection, never a caller-selected pin.

    The empty caller schema is validated before config resolution. The strict
    resolver reads this exact Config and its Standard manifest from admitted
    project content, verifies both against the installed Standard publisher,
    and replaces resolved_config before sealing subprocess stdin in the plan.
    An installed Config or an unsigned project overlay is not a fallback.
    """
    if not isinstance(params, dict) or set(params) != {"resolved_config"}:
        raise ValueError("vendor finalizer requires the admitted lock intent")
    resolved = params["resolved_config"]
    if not isinstance(resolved, dict) or set(resolved) != {"value", "source"}:
        raise ValueError("vendor lock intent requires the strict Config projection")
    source = resolved["source"]
    if (not isinstance(source, dict) or set(source) != {
            "bundle_name", "config_path", "signer_fingerprint"} or
            source["bundle_name"] != "environment-qualification" or source["config_path"] != INTENT_CONFIG or
            not isinstance(source["signer_fingerprint"], str) or
            not HASH.fullmatch(source["signer_fingerprint"])):
        raise ValueError("vendor lock intent came from an unexpected source")
    intent = resolved["value"]
    if (not isinstance(intent, dict) or set(intent) != {
            "category", "name", "version", "schema", "expected_sha256"} or
            intent["category"] != "development/ryeos" or
            intent["name"] != "cargo-vendor-lock-intent" or
            intent["schema"] != "ryeos.cargo_vendor_lock_intent.v1" or
            not isinstance(intent["version"], str) or not intent["version"] or
            not isinstance(intent["expected_sha256"], str) or
            not HASH.fullmatch(intent["expected_sha256"])):
        raise ValueError("unsupported or incomplete vendor lock intent")
    return intent["expected_sha256"]


def stable_lock(path=None):
    path = LOCK if path is None else path
    descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    try:
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode) or not 1 <= before.st_size <= MAX_LOCK_BYTES:
            raise ValueError("Cargo.lock is not one bounded regular file")
        chunks, total = [], 0
        while True:
            chunk = os.read(descriptor, min(65536, MAX_LOCK_BYTES + 1 - total))
            if not chunk:
                break
            chunks.append(chunk)
            total += len(chunk)
            if total > MAX_LOCK_BYTES:
                raise ValueError("Cargo.lock exceeds its finite bound")
        after = os.fstat(descriptor)
    finally:
        os.close(descriptor)
    identity = lambda value: (value.st_dev, value.st_ino, value.st_mode, value.st_size,
                              value.st_mtime_ns, value.st_ctime_ns)
    if identity(before) != identity(after):
        raise ValueError("Cargo.lock changed while retained")
    data = b"".join(chunks)
    digest = hashlib.sha256(data).hexdigest()
    return data, digest


def retain_lock(data, destination=None):
    destination = OUTPUT if destination is None else destination
    parent = destination.parent
    metadata = parent.lstat()
    if not stat.S_ISDIR(metadata.st_mode) or parent.is_symlink():
        raise ValueError("Cargo vendor output is not one ordinary directory")
    if destination.exists() or destination.is_symlink():
        raise ValueError("Cargo vendor output already contains a lock witness")
    descriptor, temporary = tempfile.mkstemp(prefix=".Cargo.lock.", dir=parent)
    temporary = Path(temporary)
    try:
        os.fchmod(descriptor, 0o644)
        written = 0
        while written < len(data):
            written += os.write(descriptor, data[written:])
        os.fsync(descriptor)
        os.close(descriptor)
        descriptor = -1
        # Publish without replacing another writer's witness between the
        # earlier existence check and publication. Both names share a directory.
        try:
            os.link(temporary, destination, follow_symlinks=False)
        except FileExistsError as error:
            raise ValueError("Cargo vendor output already contains a lock witness") from error
        temporary.unlink()
        directory = os.open(parent, os.O_RDONLY | os.O_CLOEXEC | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if descriptor >= 0:
            os.close(descriptor)
        if temporary.exists():
            temporary.unlink()


def execute(params, project=Path(".")):
    expected = admitted_lock_expectation(params)
    data, digest = stable_lock(project / LOCK)
    if digest != expected:
        raise ValueError("Cargo.lock differs from the admitted release lock")
    # Output selection remains the captured product root; neither the Config
    # nor caller parameters can redirect writes into source or another root.
    for parent in (project / "products", (project / OUTPUT).parent):
        if not stat.S_ISDIR(parent.lstat().st_mode) or parent.is_symlink():
            raise ValueError("Cargo vendor output is not one ordinary directory")
    retain_lock(data, project / OUTPUT)
    return {"schema": "ryeos.cargo_vendor_lock_retention.v1",
            "path": OUTPUT.as_posix(), "bytes": len(data), "sha256": digest,
            "source_mutated": False}


def main():
    if len(sys.argv) != 3 or sys.argv[1] != "--project-path":
        raise ValueError("missing admitted project context")
    project = Path(sys.argv[2])
    if not project.is_absolute():
        raise ValueError("project context must be absolute")
    raw = sys.stdin.buffer.read(65537)
    if len(raw) > 65536:
        raise ValueError("vendor finalizer request exceeds its bound")
    params = json.loads(raw)
    print(json.dumps(execute(params, project), sort_keys=True, separators=(",", ":")))


if __name__ == "__main__":
    main()
