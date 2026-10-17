# ryeos:signed:2026-09-29T03:29:28Z:5cee12bede4556d4fecb56cd859e9d748de8a5675c14df3fc94d653aba9d4503:2N8auRPbcZPIDQV8FHRlH7WbxWU/LIL04kuKk5eZkfbPMEAFGbcNPcPaXvSR5V9ub6z74XQ6CfHwPeVCa0xQCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/environments/qualification
#   version: "2.0.0"
#   description: Verify supplementary GNU static-link bytes against the admitted member profile
#   executor_id: "@subprocess"
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   filesystem_authority: captured_execution
#   network_authority: isolated
#   config_schema:
#     type: object
#     additionalProperties: false
#     required: [schema, target, files]
#     properties:
#       schema: {const: ryeos.static_link_input_profile.v2}
#       target: {const: x86_64-unknown-linux-gnu}
#       files:
#         type: object
#         minProperties: 1
#         maxProperties: 64
#         propertyNames:
#           maxLength: 256
#           pattern: '^(usr/lib/x86_64-linux-gnu/[A-Za-z0-9][A-Za-z0-9_.+-]*\.(a|o)|usr/share/doc/libc6/copyright)$'
#         additionalProperties:
#           type: object
#           additionalProperties: false
#           required: [bytes, sha256]
#           properties:
#             bytes: {type: integer, minimum: 1, maximum: 8388608}
#             sha256: {type: string, pattern: '^[0-9a-f]{64}$'}
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
HASH = re.compile(r"[0-9a-f]{64}")
# These namespaces are the fixed GNU realization layout used by release Cargo.
# Exact members belong to the independent signed policy, including versioned
# archive names. This claim measures that closure; it does not prove linkage.
LIBRARY_MEMBER = re.compile(r"usr/lib/x86_64-linux-gnu/[A-Za-z0-9][A-Za-z0-9_.+-]*\.(?:a|o)")
LICENSE_MEMBER = "usr/share/doc/libc6/copyright"


def expected_files(params):
    # The trusted policy owns expected bytes. A subject or producer testimony
    # can never replace this independently admitted input.
    if not isinstance(params, dict) or set(params) != {"schema", "target", "files"} or \
            params["schema"] != "ryeos.static_link_input_profile.v2" or \
            params["target"] != "x86_64-unknown-linux-gnu":
        raise ValueError("unsupported static-link input profile")
    entries = params["files"]
    if not isinstance(entries, dict) or not 1 <= len(entries) <= 64:
        raise ValueError("static-link input profile has an unsupported member set")
    files = {}
    total = 0
    for name, entry in entries.items():
        if not isinstance(name, str) or len(name) > 256 or (
                name != LICENSE_MEMBER and not LIBRARY_MEMBER.fullmatch(name)):
            raise ValueError("static-link input profile has an unsupported member path")
        if not isinstance(entry, dict) or set(entry) != {"bytes", "sha256"} or \
                type(entry["bytes"]) is not int or not 1 <= entry["bytes"] <= 8388608 or \
                not isinstance(entry["sha256"], str) or not HASH.fullmatch(entry["sha256"]):
            raise ValueError("static-link input profile has an invalid file expectation")
        files[name] = (entry["bytes"], entry["sha256"])
        total += entry["bytes"]
    directories = {str(parent) for name in files for parent in Path(name).parents if str(parent) != "."}
    if total > 16777216 or len(files) + len(directories) > 64:
        raise ValueError("static-link input profile exceeds product bounds")
    return files


def realizations(files):
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
    # The Tool's signed external_content declaration pins this runtime. The
    # sealed set is daemon-authored; the subject cannot select its verifier.
    if python["mount"] != "producer-python":
        raise ValueError("verifier runtime changed")
    if subject["manifest_hash"] == python["manifest_hash"] or subject["mount"] != "static-link-inputs":
        raise ValueError("subject identity changed")
    directories = {str(parent) for name in files for parent in Path(name).parents if str(parent) != "."}
    if (subject["entry_count"] != len(files) + len(directories) or
            subject["total_bytes"] != sum(size for size, _ in files.values())):
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


def inventory(root, files):
    if not stat.S_ISDIR(root.lstat().st_mode):
        raise ValueError("subject root is not a directory")
    directories_expected = {str(parent) for name in files for parent in Path(name).parents if str(parent) != "."}
    found_files, found_directories = set(), set()
    for directory, directories, members in os.walk(root, followlinks=False):
        for name in directories:
            path = Path(directory, name)
            relative = path.relative_to(root).as_posix()
            metadata = path.lstat()
            if relative not in directories_expected or not stat.S_ISDIR(metadata.st_mode) or stat.S_IMODE(metadata.st_mode) != 0o755:
                raise ValueError(f"unexpected static input directory: {relative}")
            found_directories.add(relative)
        for name in members:
            path = Path(directory, name)
            relative = path.relative_to(root).as_posix()
            if relative not in files:
                raise ValueError(f"unexpected static input file: {relative}")
            verify_file(path, *files[relative])
            found_files.add(relative)
    if found_files != set(files) or found_directories != directories_expected:
        raise ValueError("static input closure is incomplete")


def execute(params):
    files = expected_files(params)
    subject = realizations(files)
    inventory(ROOT, files)
    return {"schema": "ryeos.product_qualification_result.v1",
            "subject_manifest_hash": subject["manifest_hash"],
            "claims": ["static_link_inputs_profile_x86_64_linux_gnu_v2"],
            "probe_evidence": {
                "schema": "ryeos.static_link_input_closure_evidence.v2",
                "target": "x86_64-unknown-linux-gnu",
                "entry_count": subject["entry_count"], "total_bytes": subject["total_bytes"],
                "file_count": len(files), "network_contacted": False,
                "compiler_execution_proven": False,
                "closure_digest": hashlib.sha256(json.dumps(files, sort_keys=True, separators=(",", ":")).encode()).hexdigest()}}


if __name__ == "__main__":
    import sys
    print(json.dumps(execute(json.load(sys.stdin)), sort_keys=True, separators=(",", ":")))
