# ryeos:signed:2026-09-22T06:30:08Z:af68bd8bc225523ada20c381d07993f8cc0a8e0d03a322195c4eda9a01ed51c0:GEEo9IMFXiitgm1OBItgaHvanr6S1fUtn7UpYA9oIT5CfRA1GPRfxFnyvXDPl15rr1CK8pJsRoNU6l9pCRQcAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/development/cargo-vendor-finalize
#   version: "1.1.0"
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
import stat
import sys
import tempfile


LOCK = Path("Cargo.lock")
OUTPUT = Path("products/cargo-vendor/Cargo.lock")
EXPECTED_SHA256 = "9e8e1a93918f8e229cdbb8a037aa1a4fbbccbe5efe1257519396bb8fc3103f09"
MAX_LOCK_BYTES = 4 * 1024 * 1024


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
    if digest != EXPECTED_SHA256:
        raise ValueError("Cargo.lock differs from the admitted release lock")
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
        os.replace(temporary, destination)
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


def execute(_params, project=Path(".")):
    data, digest = stable_lock(project / LOCK)
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
    if params != {}:
        raise ValueError("vendor finalizer takes no caller parameters")
    print(json.dumps(execute(params, project), sort_keys=True, separators=(",", ":")))


if __name__ == "__main__":
    main()
