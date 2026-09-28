# ryeos:signed:2026-09-22T03:00:54Z:77fb09fd2f47d9804891a4cd62c862db9f9d1be15662955f49c6bde068e7d898:9PjtDONc966+Rp6Wu/qbUL0QZnaShAv3BnnvBPt9AbPQ+UFCaRN7f3TzfKIfBql0KUQ7YTrxz9QRBdGkQvvkBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: ryeos/environments/qualification
#   version: "1.0.2"
#   description: Independently qualify an exact offline Cargo vendor closure
#   executor_id: "@subprocess"
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
#   external_product_slots:
#     - id: subject
#       relationship_ref: config:development/ryeos/cargo-vendor-products
#       relationship: cargo_vendor_to_qualification_verifier
#       kind: tree
#       mount_root: execution_runtime
#       mount: cargo-vendor
#     - id: verifier-platform
#       relationship_ref: config:development/ryeos/platform-products
#       relationship: platform_to_cargo_vendor_verifier
#       kind: tree
#       mount_root: execution_runtime
#       mount: platform
#   env_config:
#     # The bootstrap Python is dynamically linked against its retained musl
#     # loader. Enter it through that loader; isolated execution deliberately
#     # has no ambient /lib/ld-musl-x86_64.so.1.
#     interpreter: {type: realization_member, realization_id: producer-python, relative_path: lib/ld-musl-x86_64.so.1}
#   config:
#     command: "${interpreter}"
#     args: ["--library-path", "/ryeos/realizations/producer-python/lib", "/ryeos/realizations/producer-python/python/bin/python3.14", "-I", "-B", "${tool_path}"]
#     input_data: "${params_json}"
#     timeout_secs: 120
"""Validate Cargo.lock, package checksums, and the exact offline vendor set."""

import hashlib
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import tempfile
import tomllib


ROOT = Path("/ryeos/realizations/cargo-vendor")
PRODUCER = "800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf"
HASH = re.compile(r"[0-9a-f]{64}")
MAX_FILES = 50000
MAX_FILE_BYTES = 134217728
MAX_TOTAL_BYTES = 1073741824


def realizations():
    value = json.loads(os.environ.get("RYEOS_EXTERNAL_REALIZATIONS", "null"))
    if not isinstance(value, list) or len(value) != 3:
        raise ValueError("vendor qualification requires Python, Cargo, and subject")
    by_id = {item.get("id"): item for item in value if isinstance(item, dict)}
    if set(by_id) != {"producer-python", "verifier-platform", "subject"}:
        raise ValueError("vendor qualification admitted unexpected realizations")
    python, platform, subject = by_id["producer-python"], by_id["verifier-platform"], by_id["subject"]
    keys = {"id", "kind", "mode", "manifest_hash", "entry_count", "total_bytes", "mount_root", "mount"}
    if any(set(item) != keys or not isinstance(item["entry_count"], int) or
           not isinstance(item["total_bytes"], int) for item in value):
        raise ValueError("vendor qualification realization identity is not canonical")
    if python.get("manifest_hash") != PRODUCER or python.get("mount") != "producer-python":
        raise ValueError("vendor verifier runtime changed")
    if (not HASH.fullmatch(platform.get("manifest_hash", "")) or
            platform.get("kind") != "tree" or platform.get("mode") != "pinned" or
            platform.get("mount_root") != "execution_runtime" or
            platform.get("mount") != "platform"):
        raise ValueError("vendor verifier Cargo platform changed")
    if (subject.get("kind") != "tree" or subject.get("mode") != "pinned" or
            subject.get("mount_root") != "execution_runtime" or
            subject.get("mount") != "cargo-vendor" or
            not HASH.fullmatch(subject.get("manifest_hash", ""))):
        raise ValueError("vendor subject identity changed")
    if subject["manifest_hash"] == PRODUCER:
        raise ValueError("vendor closure may not qualify itself")
    if platform["manifest_hash"] == subject["manifest_hash"]:
        raise ValueError("vendor subject may not supply its verifier platform")
    return subject, platform


def probe_offline_cargo(expected):
    # Cwd is the admitted immutable generation, never the operator's live tree.
    project = Path.cwd().resolve(strict=True)
    if bytes_stable(project / "Cargo.lock", 4 * 1024 * 1024) != bytes_stable(ROOT / "Cargo.lock", 4 * 1024 * 1024):
        raise ValueError("admitted workspace lock differs from vendor lock")
    # Cargo searches cwd and ancestors even with --manifest-path. Source config
    # must not override the selected vendor source or supply host process paths.
    for ancestor in (project, *project.parents):
        for name in ("config", "config.toml"):
            config = ancestor / ".cargo" / name
            if config.exists() or config.is_symlink():
                raise ValueError("Cargo configuration is not admitted for vendor qualification")
    bytes_stable(project / "Cargo.toml", 4 * 1024 * 1024)
    with tempfile.TemporaryDirectory(prefix="ryeos-vendor-qualification-") as directory:
        root = Path(directory)
        cargo_home, target, home, temporary = (root / name for name in ("cargo-home", "target", "home", "tmp"))
        for child in (cargo_home, target, home, temporary):
            child.mkdir(mode=0o700)
        command = ["/ryeos/realizations/platform/rust/bin/cargo",
                   "--config", 'source.crates-io.replace-with="ryeos-vendored"',
                   "--config", f'source.ryeos-vendored.directory="{ROOT}"',
                   "metadata", "--manifest-path", str(project / "Cargo.toml"),
                   "--format-version", "1", "--locked", "--frozen", "--offline"]
        environment = {"PATH": "", "HOME": str(home), "TMPDIR": str(temporary),
                       "CARGO_HOME": str(cargo_home), "CARGO_TARGET_DIR": str(target),
                       "CARGO_NET_OFFLINE": "true", "LANG": "C", "LC_ALL": "C",
                       "RUSTC": "/ryeos/realizations/platform/rust/bin/rustc"}
        completed = subprocess.run(command, cwd=project, env=environment,
                                   stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT, timeout=60, check=False)
        if completed.returncode or len(completed.stdout) > 8 * 1024 * 1024:
            raise ValueError("vendor closure is not usable by sealed offline Cargo")
        metadata = json.loads(completed.stdout)
        if not isinstance(metadata, dict) or not isinstance(metadata.get("packages"), list):
            raise ValueError("Cargo metadata lacks package evidence")
        resolved = set()
        for package in metadata["packages"]:
            if not isinstance(package, dict):
                raise ValueError("Cargo metadata package is malformed")
            source = package.get("source")
            if source is None:
                # Workspace/path packages must belong to the admitted generation.
                manifest = Path(package.get("manifest_path", "")).resolve(strict=True)
                if not manifest.is_relative_to(project):
                    raise ValueError("Cargo path package escaped admitted workspace")
                continue
            key = (package.get("name"), package.get("version"))
            if source != "registry+https://github.com/rust-lang/crates.io-index" or key not in expected or key in resolved:
                raise ValueError("Cargo resolved a package outside the retained lock")
            resolved.add(key)
        if expected and not resolved:
            raise ValueError("Cargo metadata resolved no retained registry packages")
        # Lock checks above cover every package, including optional dependencies
        # outside this workspace's default feature resolution. This digest names
        # only the observed resolution and is independent of physical paths.
        evidence = {"schema": "ryeos.cargo_vendor_registry_resolution.v1",
                    "packages": [{"name": name, "version": version, "checksum": expected[(name, version)]}
                                 for name, version in sorted(resolved)]}
        return hashlib.sha256(json.dumps(evidence, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def bytes_stable(path, maximum=MAX_FILE_BYTES):
    descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    try:
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode) or before.st_size > maximum:
            raise ValueError(f"invalid vendor member: {path}")
        chunks, total = [], 0
        while True:
            chunk = os.read(descriptor, min(65536, maximum + 1 - total))
            if not chunk:
                break
            chunks.append(chunk)
            total += len(chunk)
            if total > maximum:
                raise ValueError(f"oversized vendor member: {path}")
        after = os.fstat(descriptor)
    finally:
        os.close(descriptor)
    if (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns) != (
            after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns):
        raise ValueError(f"vendor member changed during inspection: {path}")
    return b"".join(chunks)


def package_files(root):
    result, total = {}, 0
    for directory, directories, files in os.walk(root, followlinks=False):
        directories.sort()
        files.sort()
        for name in directories + files:
            path = Path(directory, name)
            if path.is_symlink():
                raise ValueError(f"vendor closure contains a symlink: {path}")
        for name in files:
            path = Path(directory, name)
            relative = path.relative_to(root).as_posix()
            if relative == ".cargo-checksum.json":
                continue
            data = bytes_stable(path)
            total += len(data)
            if total > MAX_TOTAL_BYTES or len(result) >= MAX_FILES:
                raise ValueError("vendor closure exceeds its finite bounds")
            result[relative] = hashlib.sha256(data).hexdigest()
    return result, total


def execute(_params):
    subject, platform = realizations()
    lock_bytes = bytes_stable(ROOT / "Cargo.lock", 4 * 1024 * 1024)
    lock = tomllib.loads(lock_bytes.decode("utf-8", "strict"))
    expected = {}
    for package in lock.get("package", []):
        source = package.get("source", "")
        checksum = package.get("checksum")
        if source.startswith("registry+"):
            if not isinstance(checksum, str) or not HASH.fullmatch(checksum):
                raise ValueError("registry lock entry lacks an exact checksum")
            key = (package["name"], package["version"])
            if key in expected:
                raise ValueError("Cargo.lock repeats a registry package identity")
            expected[key] = checksum
        elif source.startswith("git+"):
            raise ValueError("offline vendor closure does not admit git dependencies")
    observed, files, total = {}, 0, 0
    for child in sorted(ROOT.iterdir(), key=lambda path: path.name.encode()):
        if child.name == "Cargo.lock":
            continue
        if not child.is_dir() or child.is_symlink():
            raise ValueError("vendor root contains an unexpected member")
        checksum_path = child / ".cargo-checksum.json"
        checksum = json.loads(bytes_stable(checksum_path, 8 * 1024 * 1024))
        package_hash = checksum.get("package")
        members = checksum.get("files")
        if not isinstance(package_hash, str) or not HASH.fullmatch(package_hash) or not isinstance(members, dict):
            raise ValueError("vendor package checksum witness is malformed")
        package_members, package_bytes = package_files(child)
        if members != package_members:
            raise ValueError("vendor package files differ from .cargo-checksum.json")
        matches = [key for key, value in expected.items()
                   if value == package_hash and child.name == f"{key[0]}-{key[1]}"]
        if len(matches) != 1 or matches[0] in observed:
            raise ValueError("vendor directory is not one exact Cargo.lock package")
        observed[matches[0]] = package_hash
        files += len(package_members)
        total += package_bytes
    if observed != expected:
        raise ValueError("vendor closure is not complete for Cargo.lock")
    cargo_registry_resolution_digest = probe_offline_cargo(expected)
    evidence = {"schema": "ryeos.cargo_vendor_evidence.v1",
                "cargo_lock_sha256": hashlib.sha256(lock_bytes).hexdigest(),
                "registry_packages": len(expected), "verified_files": files,
                "verified_bytes": total, "git_dependencies": 0,
                "cargo_registry_resolution_digest": cargo_registry_resolution_digest,
                "verifier_platform_manifest_hash": platform["manifest_hash"],
                "offline_complete": True, "network_contacted": False}
    return {"schema": "ryeos.product_qualification_result.v1",
            "subject_manifest_hash": subject["manifest_hash"],
            "claims": ["cargo_vendor_lock_closure_v1", "cargo_vendor_offline_checksums_v1"],
            "probe_evidence": evidence}


if __name__ == "__main__":
    request = json.load(__import__("sys").stdin)
    print(json.dumps(execute(request), sort_keys=True, separators=(",", ":")))
