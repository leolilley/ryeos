# ryeos:signed:2026-09-20T11:15:48Z:a5792440020aaf77a8fa51def066a14f88d34dccae73823173ead13a8256f9ec:QNdM2qBiNJTfaaL30vzpqwK/5FzenTQYiZX6JV/RrxMaRHspqRN4NlsqaQDVtt/7ZnXzHqARgFHvqyLbfHaDCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
#!/usr/bin/python3
"""Build the exact initial Core bundle from an admitted pinned source generation."""
import hashlib
import importlib.util
import json
import os
import pathlib
import re
import shutil
import stat
import subprocess
import sys
import tempfile

sys.dont_write_bytecode = True


def fail(message):
    raise SystemExit(message)


request = json.load(sys.stdin)
if not isinstance(request, dict) or set(request) != {"release_input"}:
    fail("closed Core seed build request required")
value = request["release_input"]
required = {
    "schema", "project_path", "bundle_name", "authored_manifest",
    "source_snapshot_hash", "predecessor_generation_hash", "target",
    "build_profile", "payload_ownership_item_ref",
    "payload_ownership_content_hash", "payloads", "cargo_packages",
    "build_classes", "requires_binary_build", "clean_output_required",
    "ambient_target_reuse_allowed",
}
if not isinstance(value, dict) or set(value) != required:
    fail("Core seed release input shape changed")
if value["schema"] != "ryeos.bundle_release_input_plan.v1":
    fail("Core seed release input schema changed")
if value["bundle_name"] != "core" or value["predecessor_generation_hash"] is not None:
    fail("Core seed must be the initial Core generation")
manifest_value = value["authored_manifest"]
if not isinstance(manifest_value, dict) or manifest_value.get("name") != "core":
    fail("Core seed manifest must name Core")
target = value["target"]
if (not isinstance(target, dict) or set(target) != {"kind", "triple"}
        or target.get("kind") != "triple"
        or not isinstance(target.get("triple"), str)
        or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}", target["triple"])):
    fail("Core seed requires an exact native target")
if (value["build_profile"] != "release" or not value["clean_output_required"]
        or value["ambient_target_reuse_allowed"]):
    fail("clean Core seed release build is mandatory")
if value["payload_ownership_item_ref"] != "config:bundle-release/payload-ownership":
    fail("Core seed ownership item changed")

# Execution is rooted in the admitted source generation. project_path is
# provenance only and is never used as ambient filesystem authority.
root = pathlib.Path.cwd().resolve(strict=True)
parser_path = root / "scripts/release/bundle-payload-ownership.py"
if not parser_path.is_file() or parser_path.is_symlink():
    fail("pinned ownership parser is absent or unsafe")
spec = importlib.util.spec_from_file_location("bundle_payload_ownership", parser_path)
ownership_parser = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ownership_parser)
ownership_records, ownership_identity = ownership_parser.load(root)
if ownership_identity != value["payload_ownership_content_hash"]:
    fail("Core seed ownership contract changed")
expected = sorted(
    (record for record in ownership_records if record["bundle"] == "core"),
    key=lambda item: item["binary"],
)
if value["payloads"] != expected or not expected:
    fail("Core seed is not the exact nonempty Core ownership selection")
packages = sorted({payload["cargo_package"] for payload in expected})
classes = sorted({payload["build_class"] for payload in expected})
if value["cargo_packages"] != packages or value["build_classes"] != classes:
    fail("Core seed package or build-class projection changed")
if value["requires_binary_build"] is not True:
    fail("Core seed must build its owned native payloads")
for payload in expected:
    if payload["build_class"] not in {"release", "static"}:
        fail("unknown Core payload build class")
    for field in ("binary", "cargo_package"):
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]{0,127}", payload[field]):
            fail(f"invalid owned Core {field}")

bundle_root = root / "bundles/core"
product = root / "products/core-seed/tree"
if not bundle_root.is_dir() or bundle_root.is_symlink() or product.exists():
    fail("Core source or clean product root is unsafe")
for directory, directories, files in os.walk(bundle_root, followlinks=False):
    for entry in directories + files:
        source = pathlib.Path(directory) / entry
        info = source.lstat()
        if stat.S_ISLNK(info.st_mode):
            fail("Core source contains a symbolic link")
        if not (stat.S_ISDIR(info.st_mode) or stat.S_ISREG(info.st_mode)):
            fail("Core source contains a special filesystem object")
        if stat.S_ISREG(info.st_mode) and info.st_nlink != 1:
            fail("Core source contains a hard-linked file")

def ignore_generated(directory, names):
    relative = pathlib.Path(directory).relative_to(bundle_root)
    return ["bin"] if relative == pathlib.Path(".ai") and "bin" in names else []

product.parent.mkdir(parents=True, exist_ok=False)
shutil.copytree(bundle_root, product, symlinks=False, ignore=ignore_generated)
manifest = product / ".ai/manifest.yaml"
if manifest.is_symlink():
    fail("Core manifest is unsafe")
manifest.write_text(json.dumps(manifest_value, sort_keys=True, separators=(",", ":")) + "\n")

scratch = tempfile.TemporaryDirectory(prefix="ryeos-core-seed-")
targets = {kind: pathlib.Path(scratch.name) / f"cargo-{kind}-target"
           for kind in ("release", "static")}
processes = []
triple = target["triple"]
for package in packages:
    if len({payload["build_class"] for payload in expected
            if payload["cargo_package"] == package}) != 1:
        fail("one Core Cargo package cannot cross build classes")
base_env = {
    "PATH": os.environ.get("PATH", ""),
    "HOME": os.environ.get("HOME", ""),
    "RUSTUP_HOME": os.environ.get("RUSTUP_HOME", ""),
    "CARGO_HOME": os.environ.get("CARGO_HOME", ""),
}
for build_class in ("release", "static"):
    selected = sorted({payload["cargo_package"] for payload in expected
                       if payload["build_class"] == build_class})
    if not selected:
        continue
    command = ["cargo", "build", "--release", "--locked", "--target", triple]
    for package in selected:
        command.extend(["-p", package])
    environment = {**base_env, "CARGO_TARGET_DIR": str(targets[build_class])}
    if build_class == "static":
        environment["RUSTFLAGS"] = "-C target-feature=+crt-static"
    subprocess.run(command, cwd=root, env=environment, check=True)
    processes.append({"build_class": build_class, "argv": command,
                      "cwd": str(root), "exit_code": 0})
destination_root = product / ".ai/bin" / triple
destination_root.mkdir(parents=True, exist_ok=True)
for payload in expected:
    source = targets[payload["build_class"]] / triple / "release" / payload["binary"]
    if not source.is_file() or source.is_symlink():
        fail(f"owned Core build output is absent: {payload['binary']}")
    destination = destination_root / payload["binary"]
    shutil.copyfile(source, destination)
    destination.chmod(0o755)
scratch.cleanup()
for path in product.rglob("*"):
    if path.is_symlink():
        fail("Core product contains a symbolic link")
    if path.is_file():
        path.chmod(0o755 if path.stat().st_mode & stat.S_IXUSR else 0o644)
digest = hashlib.sha256(
    json.dumps(value, sort_keys=True, separators=(",", ":")).encode()
).hexdigest()
json.dump({
    "schema": "ryeos.core_seed_build.v1",
    "release_input_digest": digest,
    "bundle_name": "core",
    "build_kind": "core_seed",
    "cargo_packages": packages,
    "output_root": "core_tree",
    "product_name": "core_seed",
    "processes": processes,
}, sys.stdout, sort_keys=True, separators=(",", ":"))
print()
