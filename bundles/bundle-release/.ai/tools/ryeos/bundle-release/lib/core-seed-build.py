#!/usr/bin/env python3
# ryeos:signed:2026-09-24T01:03:11Z:1bf338ae808a2b552e7a1077eaedcc79ae3e6e12cb2ce7c87b0e358f947b3a0f:xcGYJa6wTkLkyfQz7Lr3QCc9QoPsuTMyOOm9JAaz8Pes2b1zx2Iim0DLXF6XeNKbAZeL4z7n4vG9vjd1Dsb8Cw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Build the exact initial Core bundle from an admitted pinned source generation."""
import hashlib
import importlib.util
import json
import os
import pathlib
import re
import resource
import shutil
import stat
import subprocess
import sys
import tempfile

sys.dont_write_bytecode = True


def fail(message):
    raise SystemExit(message)


def unsigned_source_manifest(signed):
    if not isinstance(signed, bytes) or len(signed) > 1024 * 1024:
        fail("Core source manifest is unsafe or oversized")
    header, newline, body = signed.partition(b"\n")
    if not newline or not header.startswith(b"# ryeos:signed:") or not body:
        fail("Core source manifest lacks its canonical signature envelope")
    if any(line.startswith(b"# ryeos:signed:") for line in body.splitlines()):
        fail("Core source manifest contains another signature envelope")
    return body


def verified_ownership_projection(resolved):
    # The signed Tool declaration makes RyeOS resolve this Config under its
    # pinned source and publisher authority, including for direct Tool calls.
    if not isinstance(resolved, dict) or set(resolved) != {"value", "source"}:
        fail("verified ownership resolution is required")
    source = resolved["source"]
    if (not isinstance(source, dict)
            or set(source) != {"bundle_name", "config_path", "signer_fingerprint"}
            or source["bundle_name"] != "bundle-release"
            or source["config_path"] != "bundles/bundle-release/.ai/config/bundle-release/payload-ownership.yaml"
            or not isinstance(source["signer_fingerprint"], str)
            or not re.fullmatch(r"[0-9a-f]{64}", source["signer_fingerprint"])):
        fail("ownership resolution came from another source")
    document = resolved["value"]
    if (not isinstance(document, dict)
            or set(document) != {"category", "version", "description", "payload_ownership"}
            or document["category"] != "bundle-release"
            or document["version"] != "1.0.0"
            or not isinstance(document["description"], str)
            or not document["description"]):
        fail("verified ownership Config has an invalid shape")
    ownership = document["payload_ownership"]
    if (not isinstance(ownership, dict)
            or set(ownership) != {"schema", "kind", "bundles"}
            or ownership["schema"] != "ryeos.bundle_payload_ownership.v1"
            or ownership["kind"] != "bundle_payload_ownership"):
        fail("verified ownership contract has an invalid shape")
    bundles = ownership["bundles"]
    if not isinstance(bundles, list) or len(bundles) > 1024:
        fail("verified ownership bundles exceed their bound")
    canonical = json.dumps(ownership, sort_keys=True, separators=(",", ":"), ensure_ascii=True)
    if len(canonical.encode("utf-8")) > 64 * 1024:
        fail("verified ownership contract exceeds its wire bound")
    name = re.compile(r"[a-z0-9_-]{1,64}\Z")
    records, seen_binaries, previous_bundle = [], set(), None
    for bundle in bundles:
        if not isinstance(bundle, dict) or set(bundle) != {"bundle_name", "bundle_sets", "payloads"}:
            fail("verified ownership bundle has an invalid shape")
        bundle_name, sets, payloads = bundle["bundle_name"], bundle["bundle_sets"], bundle["payloads"]
        if (not isinstance(bundle_name, str) or not name.fullmatch(bundle_name)
                or previous_bundle is not None and bundle_name <= previous_bundle):
            fail("verified ownership bundle names are not unique and sorted")
        previous_bundle = bundle_name
        if (not isinstance(sets, list) or not sets
                or any(not isinstance(item, str) or not name.fullmatch(item) for item in sets)
                or sets != sorted(set(sets))):
            fail("verified ownership bundle sets are not unique and sorted")
        if not isinstance(payloads, list) or not payloads:
            fail("data-only bundles must be absent from ownership")
        previous_binary = None
        for payload in payloads:
            if not isinstance(payload, dict) or set(payload) != {"binary", "cargo_package", "build_class"}:
                fail("verified owned payload has an invalid shape")
            binary, package, build_class = payload["binary"], payload["cargo_package"], payload["build_class"]
            if (not isinstance(binary, str) or not name.fullmatch(binary)
                    or previous_binary is not None and binary <= previous_binary
                    or binary in seen_binaries):
                fail("verified owned binaries are not globally unique and sorted")
            if (not isinstance(package, str) or not name.fullmatch(package)
                    or build_class not in {"release", "static"}):
                fail("verified owned payload has invalid package or class")
            previous_binary = binary
            seen_binaries.add(binary)
            records.append({"bundle":bundle_name,"binary":binary,"cargo_package":package,
                            "build_class":build_class,"bundle_sets":sets})
            if len(records) > 256:
                fail("verified owned payloads exceed their bound")
    return records, hashlib.sha256(canonical.encode("utf-8")).hexdigest()


# This helper is part of the authenticated Tool source closure, not project code.
elf_spec = importlib.util.spec_from_file_location(
    "release_elf", pathlib.Path(__file__).with_name("release-elf.py"))
release_elf = importlib.util.module_from_spec(elf_spec)
elf_spec.loader.exec_module(release_elf)


request = json.load(sys.stdin)
if not isinstance(request, dict) or set(request) != {"release_input", "resolved_config"}:
    fail("closed Core seed build request and verified ownership resolution required")
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
ownership_records, ownership_identity = verified_ownership_projection(request["resolved_config"])
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
# The admitted CoW project hard-links verified content-cache inodes into its
# lower tree. Source link count is not an authored-tree safety check here;
# shutil.copytree creates private output inodes below.

def ignore_generated(directory, names):
    relative = pathlib.Path(directory).relative_to(bundle_root)
    return ["bin"] if relative == pathlib.Path(".ai") and "bin" in names else []

product.parent.mkdir(parents=True, exist_ok=False)
shutil.copytree(bundle_root, product, symlinks=False, ignore=ignore_generated)
manifest = product / ".ai/manifest.yaml"
if manifest.is_symlink():
    fail("Core manifest is unsafe")
manifest.write_bytes(unsigned_source_manifest(manifest.read_bytes()))

resource.setrlimit(resource.RLIMIT_NOFILE, (4096, 4096))
resource.setrlimit(resource.RLIMIT_NPROC, (512, 512))
resource.setrlimit(resource.RLIMIT_FSIZE, (1 << 30, 1 << 30))
scratch = tempfile.TemporaryDirectory(prefix=".ryeos-core-seed-", dir=root)
targets = {kind: pathlib.Path(scratch.name) / f"cargo-{kind}-target"
           for kind in ("release", "static")}
processes = []
payload_transforms = []
triple = target["triple"]
for package in packages:
    if len({payload["build_class"] for payload in expected
            if payload["cargo_package"] == package}) != 1:
        fail("one Core Cargo package cannot cross build classes")
source_cargo = root / ".cargo"
if source_cargo.exists() or source_cargo.is_symlink():
    fail("release source may not provide Cargo configuration")
private_root = pathlib.Path(scratch.name) / "environment"
cargo_spec = importlib.util.spec_from_file_location(
    "release_cargo", pathlib.Path(__file__).with_name("release-cargo.py"))
release_cargo = importlib.util.module_from_spec(cargo_spec)
cargo_spec.loader.exec_module(release_cargo)
release_cargo.validate_source_configuration(root)
platform = release_cargo.PLATFORM
cargo = "/ryeos/realizations/platform/rust/bin/cargo"
base_env = release_cargo.build_environment(
    private_root, triple, "/ryeos/realizations/static-link-inputs")
for build_class in ("release", "static"):
    selected = sorted({payload["cargo_package"] for payload in expected
                       if payload["build_class"] == build_class})
    if not selected:
        continue
    # A shared package may contain non-Core binaries. Do not build them merely
    # because Core owns another binary from that package.
    for package in selected:
        command = [cargo, "--config", 'source.crates-io.replace-with="ryeos-vendored"',
                   "--config", 'source.ryeos-vendored.directory="/ryeos/realizations/cargo-vendor"',
                   "build", "--release", "--locked", "--frozen", "--offline",
                   "--jobs", "2", "--target", triple, "-p", package]
        for binary in sorted(payload["binary"] for payload in expected
                             if payload["cargo_package"] == package
                             and payload["build_class"] == build_class):
            command.extend(["--bin", binary])
        environment = {**base_env, "CARGO_TARGET_DIR": str(targets[build_class])}
        if build_class == "static":
            environment["RUSTFLAGS"] += " -C target-feature=+crt-static"
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
    transformation = release_elf.normalize_output_elf(destination, payload["build_class"], platform)
    payload_transforms.append({"binary": payload["binary"], **transformation})
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
    "payload_transforms": payload_transforms,
}, sys.stdout, sort_keys=True, separators=(",", ":"))
print()
