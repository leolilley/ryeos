# ryeos:signed:2026-09-18T02:33:38Z:9956abc6885a614443b15c76f44aca33bbe03934dcad469ef0a521cb26e034ab:DTYdSAbk2VAsVcjEjD1pUkiFpwOrTM4T+ibxbXMJw9iSl7kD16t5QPEIAwH6b8bfAdZLdCtOBJmOiVM/nWz3DQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/environments/qualification
#   version: "1.0.0"
#   description: Independently reproduce the exact authoring source-input manifest
#   executor_id: tool:ryeos/environments/qualification/native-authoring/runtime
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   filesystem_authority: captured_execution
#   network_authority: isolated
#   config_schema:
#     type: object
#     properties: {}
#     additionalProperties: false
#   external_content:
#     - id: authoring-source-inputs
#       kind: tree
#       mode: pinned
#       digest: c66ac1c984e0793416106cd2fefa1a45d1b00c17b5865ff751e41594a3d39857
#       metadata_hint: ryeos-authoring-source-inputs-v2-large-content
#       mount_root: execution_runtime
#       mount: authoring-source-inputs
"""Projectless verifier for the raw authoring source-input realization."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import stat


REALIZATION_ID = "authoring-source-inputs"
MANIFEST = "c66ac1c984e0793416106cd2fefa1a45d1b00c17b5865ff751e41594a3d39857"
ROOT = Path("/ryeos/realizations/authoring-source-inputs")
ENTRY_COUNT = 55
TOTAL_BYTES = 247_886_376
CONTENT_FILE_LIMIT = 32 * 1024 * 1024
LARGE_CHUNK_BYTES = 64 * 1024 * 1024
MAX_REALIZATION_WIRE_BYTES = 16 * 1024


def canonical_json(value: object) -> bytes:
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=True
    ).encode("ascii")


def hash_file(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def reproduce_manifest() -> tuple[int, int, str]:
    if ROOT.is_symlink() or not ROOT.is_dir():
        raise ValueError("authoring source-input root is not an ordinary directory")
    entries = []
    total = 0
    for path in sorted(ROOT.rglob("*"), key=lambda item: item.relative_to(ROOT).as_posix().encode()):
        name = path.relative_to(ROOT).as_posix()
        info = path.lstat()
        if stat.S_ISDIR(info.st_mode):
            entries.append({"path": name, "kind": "dir"})
            continue
        if not stat.S_ISREG(info.st_mode):
            raise ValueError("authoring source-input tree contains a link or special file")
        mode = 0o755 if info.st_mode & 0o111 else 0o644
        total += info.st_size
        entry = {"path": name, "kind": "file", "mode": mode, "size": info.st_size}
        if info.st_size <= CONTENT_FILE_LIMIT:
            entry["blob_hash"] = hash_file(path)
        else:
            whole = hashlib.sha256()
            chunks = []
            with path.open("rb") as stream:
                while chunk := stream.read(LARGE_CHUNK_BYTES):
                    whole.update(chunk)
                    chunks.append(hashlib.sha256(chunk).hexdigest())
            entry.update(
                file_sha256=whole.hexdigest(),
                chunk_size=LARGE_CHUNK_BYTES,
                chunk_hashes=chunks,
            )
        entries.append(entry)
    value = {
        "schema": "ryeos.external_content.large.v2",
        "kind": "external_large_content_manifest",
        "entries": entries,
        "entry_count": len(entries),
        "total_bytes": total,
    }
    return len(entries), total, hashlib.sha256(canonical_json(value)).hexdigest()


def sealed_realization() -> dict:
    raw = os.environ.get("RYEOS_EXTERNAL_REALIZATIONS", "")
    if not raw or len(raw.encode()) > MAX_REALIZATION_WIRE_BYTES:
        raise ValueError("missing or oversized sealed realization set")
    value = json.loads(raw)
    if not isinstance(value, list) or len(value) != 1 or not isinstance(value[0], dict):
        raise ValueError("expected exactly one sealed realization")
    item = value[0]
    required = {
        "id", "kind", "mode", "manifest_hash", "entry_count", "total_bytes",
        "mount_root", "mount",
    }
    if set(item) != required or (
        item["id"] != REALIZATION_ID
        or item["kind"] != "tree"
        or item["mode"] != "pinned"
        or item["manifest_hash"] != MANIFEST
        or item["entry_count"] != ENTRY_COUNT
        or item["total_bytes"] != TOTAL_BYTES
        or item["mount_root"] != "execution_runtime"
        or item["mount"] != REALIZATION_ID
    ):
        raise ValueError("sealed authoring source-input identity or shape changed")
    return item


def main() -> None:
    sealed = sealed_realization()
    entries, total, digest = reproduce_manifest()
    if (entries, total, digest) != (ENTRY_COUNT, TOTAL_BYTES, MANIFEST):
        raise ValueError("authoring source-input bytes differ from their admitted identity")
    print(json.dumps({
        "schema": "ryeos.external_content_qualification.v1",
        "qualified": True,
        "realization_id": REALIZATION_ID,
        "manifest_digest": sealed["manifest_hash"],
        "entry_count": entries,
        "total_bytes": total,
    }, sort_keys=True))


if __name__ == "__main__":
    main()
