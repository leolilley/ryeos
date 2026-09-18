# ryeos:signed:2026-09-18T02:33:39Z:3373034684fe2abb52a2b6f78e4c6cb127851e4a36101cbb10cc442fcda08210:iZ5DUAtupOREID89s6Wl/DdRh/pv3dzo0+E8ldD6P8tTmWcApaxDsg9ZTUe1Ou6kCAkW7zIv2kq3+1H9I6k9AQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/environments/qualification
#   version: "1.0.0"
#   description: Independently verify the exact GNU Python producer archive input tree
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
#     - id: gnu-python-archives
#       kind: tree
#       mode: pinned
#       digest: ec7d4a2a749b0b709dabca0a7919c3a897a9b671be56332b67a659b6bd82cac6
#       mount_root: execution_runtime
#       mount: gnu-python-archives
"""Projectless verifier for the GNU Python producer's literal input tree."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import stat


REALIZATION_ID = "gnu-python-archives"
MANIFEST = "ec7d4a2a749b0b709dabca0a7919c3a897a9b671be56332b67a659b6bd82cac6"
ROOT = Path("/ryeos/realizations/gnu-python-archives")
MAX_REALIZATION_WIRE_BYTES = 16 * 1024
HASH = re.compile(r"[0-9a-f]{64}")
FILES = {
    "cpython-gnu-install.tar.gz": (
        35_940_499,
        "a2478d654ed51d443bae21ec20ad927f116b4f5aae4094ab74918a6aa38f0575",
    ),
    "cpython-gnu-full.tar.zst": (
        127_731_089,
        "eb419a7d8d526e1372b5e3db6478176d48380832dffb14fd5a8565a23227901a",
    ),
    "cpython-source-deps-zstd-1.5.7.tar.gz": (
        2_440_298,
        "f24b52470d12f466e9fa4fcc94e6c530625ada51d7b36de7fdc6ed7e6f499c8e",
    ),
    "upstream-build-zlib.sh": (
        761,
        "58e58a24f79cec8ecfc250adcce8ec829753e59cdfd54cf4de4d37018c45c0f9",
    ),
}


def file_sha256(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def sealed_realization() -> dict:
    raw = os.environ.get("RYEOS_EXTERNAL_REALIZATIONS", "")
    if not raw or len(raw.encode()) > MAX_REALIZATION_WIRE_BYTES:
        raise ValueError("missing or oversized sealed realization set")
    value = json.loads(raw)
    if not isinstance(value, list) or len(value) != 1:
        raise ValueError("expected exactly one sealed realization")
    item = value[0]
    required = {
        "id", "kind", "mode", "manifest_hash", "entry_count", "total_bytes",
        "mount_root", "mount",
    }
    if not isinstance(item, dict) or set(item) != required:
        raise ValueError("sealed realization has an incomplete shape")
    if (
        item["id"] != REALIZATION_ID
        or item["kind"] != "tree"
        or item["mode"] != "pinned"
        or item["manifest_hash"] != MANIFEST
        or not HASH.fullmatch(item["manifest_hash"])
        or item["entry_count"] != len(FILES)
        or item["total_bytes"] != sum(size for size, _ in FILES.values())
        or item["mount_root"] != "execution_runtime"
        or item["mount"] != REALIZATION_ID
    ):
        raise ValueError("sealed realization identity or shape changed")
    return item


def main() -> None:
    sealed = sealed_realization()
    if ROOT.is_symlink() or not ROOT.is_dir():
        raise ValueError("GNU Python archive realization root is not a directory")
    observed = set()
    for path in ROOT.iterdir():
        info = path.lstat()
        if not stat.S_ISREG(info.st_mode) or path.name not in FILES:
            raise ValueError("GNU Python archive realization has an unexpected member")
        expected_size, expected_hash = FILES[path.name]
        if info.st_size != expected_size or file_sha256(path) != expected_hash:
            raise ValueError(f"GNU Python archive input changed: {path.name}")
        observed.add(path.name)
    if observed != set(FILES):
        raise ValueError("GNU Python archive realization is incomplete")
    print(json.dumps({
        "schema": "ryeos.external_content_qualification.v1",
        "qualified": True,
        "realization_id": REALIZATION_ID,
        "manifest_digest": sealed["manifest_hash"],
        "entry_count": sealed["entry_count"],
        "total_bytes": sealed["total_bytes"],
    }, sort_keys=True))


if __name__ == "__main__":
    main()
