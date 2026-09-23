#!/usr/bin/env python3
# ryeos:signed:2026-09-23T08:19:54Z:6c08992e405e76e7027327494e6ef3d2df7a63fccb1056d3aadc9caab7eaaeae:WMATbv4SyB6RuZ97EJ8Wq44e8WMmxQJxrVYIQ1p5+Ve7YaCCrQhj5mhprtY3SRp4aKLAuO943VJBlXtZEcoeBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Build one exact non-core bundle selected by the ownership contract."""
import hashlib, importlib.util, json, os, pathlib, re, resource, shutil, stat, subprocess, sys, tempfile
sys.dont_write_bytecode = True

def fail(message): raise ValueError(message)

# This helper is part of the authenticated Tool source closure, not project code.
elf_spec = importlib.util.spec_from_file_location(
    "release_elf", pathlib.Path(__file__).with_name("release-elf.py"))
release_elf = importlib.util.module_from_spec(elf_spec)
elf_spec.loader.exec_module(release_elf)
request = json.load(sys.stdin)
if set(request) != {"release_input"}: fail("closed build request required")
value = request["release_input"]
required = {"schema","project_path","bundle_name","authored_manifest","source_snapshot_hash","predecessor_generation_hash",
            "target","build_profile","payload_ownership_item_ref","payload_ownership_content_hash",
            "payloads","cargo_packages",
            "build_classes","requires_binary_build","clean_output_required","ambient_target_reuse_allowed"}
if not isinstance(value, dict) or set(value) != required: fail("release input shape changed")
if value["build_profile"] != "release" or not value["clean_output_required"] or value["ambient_target_reuse_allowed"]:
    fail("clean release build is mandatory")
# Source reads are rooted in the admitted pinned execution generation. The
# original path remains provenance only and is never ambient build authority.
root = pathlib.Path.cwd().resolve(strict=True)
parser_path = root / "scripts/release/bundle-payload-ownership.py"
spec = importlib.util.spec_from_file_location("bundle_payload_ownership", parser_path)
ownership_parser = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ownership_parser)
ownership_records, ownership_identity = ownership_parser.load(root)
if value["payload_ownership_item_ref"] != "config:bundle-release/payload-ownership":
    fail("ownership config item reference changed")
if ownership_identity != value["payload_ownership_content_hash"]:
    fail("ownership contract changed")
name = value["bundle_name"]
if name == "core": fail("core is substrate-owned and cannot use bundle-only publication")
if not isinstance(name, str) or not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,127}", name): fail("invalid bundle name")
expected = [record for record in ownership_records if record["bundle"] == name]
expected.sort(key=lambda item: item["binary"])
payloads = value["payloads"]
if payloads != expected: fail("release input is not the exact ownership selection")
packages = sorted({payload["cargo_package"] for payload in expected})
classes = sorted({payload["build_class"] for payload in expected})
if value["cargo_packages"] != packages or value["build_classes"] != classes: fail("release input package or build-class projection is incorrect")
if value["requires_binary_build"] != bool(expected): fail("release input binary-build decision is incorrect")
for payload in expected:
    if payload["build_class"] not in {"release", "static"}: fail("unknown payload build class")
    for field in ("binary", "cargo_package"):
        if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9_-]{0,127}", payload[field]): fail(f"invalid owned {field}")
workspace = pathlib.Path.cwd()
product = workspace / "products/native-bundle/tree"
resource.setrlimit(resource.RLIMIT_NOFILE, (4096, 4096))
resource.setrlimit(resource.RLIMIT_NPROC, (512, 512))
resource.setrlimit(resource.RLIMIT_FSIZE, (1 << 30, 1 << 30))
scratch = tempfile.TemporaryDirectory(prefix=".ryeos-native-build-", dir=workspace)
targets = {kind: pathlib.Path(scratch.name) / f"cargo-{kind}-target" for kind in ("release", "static")}
if product.exists() or any(target.exists() for target in targets.values()): fail("clean product roots already exist")
product.parent.mkdir(parents=True, exist_ok=False)
def exclude_generated(directory, names):
    relative = pathlib.Path(directory).relative_to(root / "bundles" / name)
    return ["bin"] if relative == pathlib.Path(".ai") and "bin" in names else []
bundle_root = root / "bundles" / name
if not bundle_root.is_dir() or bundle_root.is_symlink(): fail("selected bundle source is absent or unsafe")
for directory, directories, files in os.walk(bundle_root, followlinks=False):
    for entry in directories + files:
        source_entry = pathlib.Path(directory) / entry
        metadata = source_entry.lstat()
        if stat.S_ISLNK(metadata.st_mode): fail(f"bundle source contains a symbolic link: {source_entry.relative_to(bundle_root)}")
        if not (stat.S_ISDIR(metadata.st_mode) or stat.S_ISREG(metadata.st_mode)):
            fail(f"bundle source contains a special file: {source_entry.relative_to(bundle_root)}")
        if stat.S_ISREG(metadata.st_mode) and metadata.st_nlink != 1:
            fail(f"bundle source contains a hard-linked file: {source_entry.relative_to(bundle_root)}")
shutil.copytree(bundle_root, product, symlinks=False, ignore=exclude_generated)
manifest = product / ".ai/manifest.yaml"
authored_manifest = value["authored_manifest"]
if not isinstance(authored_manifest, dict) or authored_manifest.get("name") != name:
    fail("authored manifest names another bundle")
if manifest.is_symlink(): fail("bundle manifest is unsafe")
# The authenticated build handler regenerates this with RyeOS's canonical
# source-manifest materializer before dispatch; the publisher signs it later.
manifest.write_text(json.dumps(authored_manifest, sort_keys=True, separators=(",", ":")) + "\n")
processes = []
payload_transforms = []
if expected:
    triple = value["target"].get("triple") if value["target"].get("kind") == "triple" else None
    if not triple: fail("exact target triple required")
    for package in packages:
        if len({payload["build_class"] for payload in expected if payload["cargo_package"] == package}) != 1:
            fail("one Cargo package cannot cross build classes")
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
        selected = sorted({payload["cargo_package"] for payload in expected if payload["build_class"] == build_class})
        if not selected: continue
        # Package ownership is not binary ownership: handler-bins is shared
        # across bundles. Select only this bundle's bins, within one package.
        for package in selected:
            command = [cargo, "--config", 'source.crates-io.replace-with="ryeos-vendored"',
                       "--config", 'source.ryeos-vendored.directory="/ryeos/realizations/cargo-vendor"',
                       "build", "--release", "--locked", "--frozen", "--offline",
                       "--jobs", "2", "--target", triple, "-p", package]
            for binary in sorted(payload["binary"] for payload in expected
                                 if payload["cargo_package"] == package
                                 and payload["build_class"] == build_class):
                command.extend(["--bin", binary])
            env = {**base_env, "CARGO_TARGET_DIR":str(targets[build_class])}
            if build_class == "static": env["RUSTFLAGS"] += " -C target-feature=+crt-static"
            subprocess.run(command, cwd=root, env=env, check=True)
            processes.append({"build_class":build_class,"argv":command,"cwd":str(root),"exit_code":0})
    destination_root = product / ".ai/bin" / triple
    destination_root.mkdir(parents=True, exist_ok=True)
    for payload in expected:
        source = targets[payload["build_class"]] / triple / "release" / payload["binary"]
        if not source.is_file() or source.is_symlink(): fail(f"owned build output is absent: {payload['binary']}")
        destination = destination_root / payload["binary"]
        shutil.copyfile(source, destination)
        transformation = release_elf.normalize_output_elf(destination, payload["build_class"], platform)
        payload_transforms.append({"binary": payload["binary"], **transformation})
        destination.chmod(0o755)
elif value["target"] != {"kind":"portable"}: fail("data-only bundle requires the portable target")
scratch.cleanup()
for path in product.rglob("*"):
    if path.is_symlink(): fail("bundle product contains a symbolic link")
    if path.is_file(): path.chmod(0o755 if path.stat().st_mode & stat.S_IXUSR else 0o644)
digest = hashlib.sha256(json.dumps(value,sort_keys=True,separators=(",",":")).encode()).hexdigest()
json.dump({"schema":"ryeos.native_bundle_build.v1","release_input_digest":digest,
           "bundle_name":name,"build_kind":"native" if expected else "data_only",
           "cargo_packages":packages,"output_root":"bundle_tree",
           "product_name":"native_bundle","processes":processes,
           "payload_transforms":payload_transforms},
          sys.stdout,sort_keys=True,separators=(",",":")); print()
