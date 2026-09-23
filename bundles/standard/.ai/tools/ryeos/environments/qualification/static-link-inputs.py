# ryeos:signed:2026-09-22T04:12:14Z:8494d26898e58c0dfa664e6731ed38b3f09f75a4c742379f3fc10826c73d8468:cRxxD9OQcf7ERVBydvpnxpjXO/r5Mz4kwrC3km4XDvr+FUnSpa699ahRWW50JmJPY2dxCz87XvOVIVIqyeoHBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/environments/qualification
#   version: "1.0.0"
#   description: Independently verify the exact supplementary static-link input closure
#   executor_id: "@subprocess"
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   filesystem_authority: captured_execution
#   network_authority: isolated
#   config_schema: {type: object, properties: {}, additionalProperties: false}
#   external_content:
#     - id: producer-python
#       kind: tree
#       mode: pinned
#       digest: 800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf
#       mount_root: execution_runtime
#       mount: producer-python
#   external_product_slots:
#     - id: subject
#       relationship_ref: config:development/ryeos/static-link-input-products
#       relationship: static_link_inputs_to_qualification_verifier
#       kind: tree
#       mount_root: execution_runtime
#       mount: static-link-inputs
#   env_config:
#     interpreter: {type: realization_member, realization_id: producer-python, relative_path: lib/ld-musl-x86_64.so.1}
#   config:
#     command: "${interpreter}"
#     args: ["--library-path", "/ryeos/realizations/producer-python/lib", "/ryeos/realizations/producer-python/python/bin/python3.14", "-I", "-B", "${tool_path}"]
#     input_data: "${params_json}"
#     timeout_secs: 120
"""Exact checksum closure qualification, not a compiler execution proof."""
import hashlib
import json
import os
from pathlib import Path
import re
import stat

ROOT = Path("/ryeos/realizations/static-link-inputs")
PRODUCER = "800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf"
HASH = re.compile(r"[0-9a-f]{64}")
EMPTY_ARCHIVE = "f0a17a43c74d2fe5474fa2fd29c8f14799e777d7d75a2cc4d11c20a6e7b161c5"
# Independent verifier pins: do not load these from the producer or subject.
FILES = {
    "usr/lib/x86_64-linux-gnu/libc.a": (5669090, "421b7c3395b35301a4626593b35842849ae1ba43582320383ffbc9790d918515"),
    "usr/lib/x86_64-linux-gnu/libm.a": (132, "aef85c72b98fbeb021f6bdd3abb137e62643d7970bd59f84dc1a7c43fa6b9f14"),
    "usr/lib/x86_64-linux-gnu/libm-2.41.a": (2502842, "0d748002785b531c0aa84679682deee556828ab21f9912569edd6bfae484ed0e"),
    "usr/lib/x86_64-linux-gnu/libmvec.a": (1762900, "60f1a89b047a78bbd64843d1e388b8b7ee6f4d6b9faf425f0f21f1617a8dbd4f"),
    "usr/lib/x86_64-linux-gnu/libpthread.a": (8, EMPTY_ARCHIVE),
    "usr/lib/x86_64-linux-gnu/libdl.a": (8, EMPTY_ARCHIVE),
    "usr/lib/x86_64-linux-gnu/librt.a": (8, EMPTY_ARCHIVE),
    "usr/lib/x86_64-linux-gnu/libutil.a": (8, EMPTY_ARCHIVE),
    "usr/lib/x86_64-linux-gnu/rcrt1.o": (1640, "4ca31259aee8415b60fe566c00cecfa5858f1103296b8043f175ccf2489f00ec"),
    "usr/lib/x86_64-linux-gnu/crt1.o": (1776, "8cd1a50edd44a4ded5856bf2254dc106401b37c285dcc55a594475438cd6ec1a"),
    "usr/share/doc/libc6/copyright": (51200, "f5788886720a2605a946e81d571e6c8162b09f58d2e2ceb8d36e5768fccd850d"),
}
DIRECTORIES = {str(parent) for name in FILES for parent in Path(name).parents if str(parent) != "."}


def realizations():
    value = json.loads(os.environ.get("RYEOS_EXTERNAL_REALIZATIONS", "null"))
    keys = {"id", "kind", "mode", "manifest_hash", "entry_count", "total_bytes", "mount_root", "mount"}
    if not isinstance(value, list) or len(value) != 2:
        raise ValueError("qualification requires verifier and subject realizations")
    for item in value:
        if (not isinstance(item, dict) or set(item) != keys or
                type(item["entry_count"]) is not int or item["entry_count"] <= 0 or
                type(item["total_bytes"]) is not int or item["total_bytes"] <= 0 or
                item["kind"] != "tree" or item["mode"] != "pinned" or
                item["mount_root"] != "execution_runtime" or
                not isinstance(item["manifest_hash"], str) or not HASH.fullmatch(item["manifest_hash"])):
            raise ValueError("noncanonical realization identity")
    by_id = {item["id"]: item for item in value}
    if set(by_id) != {"producer-python", "subject"}:
        raise ValueError("unexpected realization identity")
    python, subject = by_id["producer-python"], by_id["subject"]
    if python["manifest_hash"] != PRODUCER or python["mount"] != "producer-python":
        raise ValueError("verifier runtime changed")
    if subject["manifest_hash"] == PRODUCER or subject["mount"] != "static-link-inputs":
        raise ValueError("subject identity changed")
    if (subject["entry_count"] != len(FILES) + len(DIRECTORIES) or
            subject["total_bytes"] != sum(size for size, _ in FILES.values())):
        raise ValueError("subject exceeds exact closure bounds")
    return subject


def verify_file(path, size, digest):
    descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        before = os.fstat(descriptor)
        if (not stat.S_ISREG(before.st_mode) or before.st_nlink != 1 or
                stat.S_IMODE(before.st_mode) != 0o644 or before.st_size != size):
            raise ValueError(f"invalid static input metadata: {path}")
        actual, length = hashlib.sha256(), 0
        while length <= size:
            chunk = os.read(descriptor, min(65536, size + 1 - length))
            if not chunk:
                break
            actual.update(chunk)
            length += len(chunk)
        after = os.fstat(descriptor)
        identity = lambda metadata: (metadata.st_dev, metadata.st_ino, metadata.st_size,
                                     metadata.st_mode, metadata.st_nlink,
                                     metadata.st_mtime_ns, metadata.st_ctime_ns)
        if (identity(before) != identity(after) or length != size or actual.hexdigest() != digest):
            raise ValueError(f"static input changed or checksum differs: {path}")
    finally:
        os.close(descriptor)


def inventory(root):
    if not stat.S_ISDIR(root.lstat().st_mode):
        raise ValueError("subject root is not a directory")
    found_files, found_directories = set(), set()
    for directory, directories, files in os.walk(root, followlinks=False):
        for name in directories:
            path = Path(directory, name)
            relative = path.relative_to(root).as_posix()
            metadata = path.lstat()
            if relative not in DIRECTORIES or not stat.S_ISDIR(metadata.st_mode) or stat.S_IMODE(metadata.st_mode) != 0o755:
                raise ValueError(f"unexpected static input directory: {relative}")
            found_directories.add(relative)
        for name in files:
            path = Path(directory, name)
            relative = path.relative_to(root).as_posix()
            if relative not in FILES:
                raise ValueError(f"unexpected static input file: {relative}")
            verify_file(path, *FILES[relative])
            found_files.add(relative)
    if found_files != set(FILES) or found_directories != DIRECTORIES:
        raise ValueError("static input closure is incomplete")


def execute(params):
    if params != {}:
        raise ValueError("qualification accepts no parameters")
    subject = realizations()
    inventory(ROOT)
    return {"schema": "ryeos.product_qualification_result.v1",
            "subject_manifest_hash": subject["manifest_hash"],
            "claims": ["static_link_inputs_x86_64_linux_gnu_v1"],
            "probe_evidence": {
                "schema": "ryeos.static_link_input_closure_evidence.v1",
                "target": "x86_64-unknown-linux-gnu",
                "entry_count": subject["entry_count"], "total_bytes": subject["total_bytes"],
                "file_count": len(FILES), "network_contacted": False,
                "compiler_execution_proven": False,
                "closure_digest": hashlib.sha256(json.dumps(FILES, sort_keys=True, separators=(",", ":")).encode()).hexdigest()}}


if __name__ == "__main__":
    import sys
    print(json.dumps(execute(json.load(sys.stdin)), sort_keys=True, separators=(",", ":")))
