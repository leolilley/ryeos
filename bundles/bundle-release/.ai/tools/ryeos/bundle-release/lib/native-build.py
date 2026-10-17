#!/usr/bin/env python3
# ryeos:signed:2026-10-15T23:47:07Z:815be5ec5404ac5e665a1ea9e60a830e54c2e0c06b9b230c07a6720da62ed14e:t95ozJebzK8uIzv8xQ773JGKsOgX4pyS1kG0eHbU6Q2KqoBbeIwRGE3jsOLBLYNyddkc8DWrFj/zkaEX+tu+BA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Build one exact non-core bundle selected by the ownership contract."""
import hashlib, importlib.util, json, os, pathlib, re, resource, shutil, stat, subprocess, sys, tempfile
sys.dont_write_bytecode = True

def fail(message): raise ValueError(message)

def feature_selections(records, implications):
    selected = {}
    for record in records:
        selected.setdefault(record["cargo_package"], set()).update(record["required_features"])
    result = []
    for package, features in sorted(selected.items()):
        implied = set()
        for feature in features:
            implied.update(implications.get(package, {}).get(feature, []))
        result.append({"cargo_package": package, "features": sorted(features - implied)})
    return result

def verified_ownership_projection(resolved):
    # The signed Tool declaration makes RyeOS resolve this Config under its
    # pinned source and publisher authority. This value is injected at launch,
    # replacing any caller-supplied resolved_config, even for a direct Tool call.
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
            or document["version"] != "2.0.0"
            or not isinstance(document["description"], str)
            or not document["description"]):
        fail("verified ownership Config has an invalid shape")
    ownership = document["payload_ownership"]
    if (not isinstance(ownership, dict)
            or set(ownership) != {"schema", "kind", "host_packages", "no_default_features_packages", "feature_implications",
                                  "data_only_bundles", "host_only_bundles", "bundles"}
            or ownership["schema"] != "ryeos.bundle_payload_ownership.v2"
            or ownership["kind"] != "bundle_payload_ownership"):
        fail("verified ownership contract has an invalid shape")
    bundles = ownership["bundles"]
    if not isinstance(bundles, list) or len(bundles) > 1024:
        fail("verified ownership bundles exceed their bound")
    canonical = json.dumps(ownership, sort_keys=True, separators=(",", ":"), ensure_ascii=True)
    if len(canonical.encode("utf-8")) > 64 * 1024:
        fail("verified ownership contract exceeds its wire bound")
    name = re.compile(r"[a-z0-9_-]{1,64}\Z")
    def sorted_names(items, allow_empty):
        return (isinstance(items, list) and (allow_empty or bool(items))
                and all(isinstance(item, str) and name.fullmatch(item) for item in items)
                and items == sorted(set(items)))
    if not sorted_names(ownership["host_packages"], True):
        fail("verified ownership host packages are not unique and sorted")
    if not sorted_names(ownership["no_default_features_packages"], True):
        fail("verified ownership no-default-features packages are not unique and sorted")
    implications = ownership["feature_implications"]
    if not isinstance(implications, dict):
        fail("verified ownership feature implications have an invalid shape")
    for package, table in implications.items():
        if not isinstance(package, str) or not name.fullmatch(package) or not isinstance(table, dict):
            fail("verified ownership feature implications have an invalid shape")
        for feature, implied in table.items():
            if (not isinstance(feature, str) or not name.fullmatch(feature)
                    or not sorted_names(implied, False) or feature in implied):
                fail("verified ownership feature implications are not unique and sorted")
    if not sorted_names(ownership["data_only_bundles"], True):
        fail("verified ownership data-only bundles are not unique and sorted")
    host_only = ownership["host_only_bundles"]
    if not isinstance(host_only, list):
        fail("verified ownership host-only bundles have an invalid shape")
    previous_host_only = None
    for entry in host_only:
        if not isinstance(entry, dict) or set(entry) != {"bundle_name", "bundle_sets", "host_features"}:
            fail("verified ownership host-only bundle has an invalid shape")
        if (not isinstance(entry["bundle_name"], str) or not name.fullmatch(entry["bundle_name"])
                or previous_host_only is not None and entry["bundle_name"] <= previous_host_only):
            fail("verified ownership host-only bundle names are not unique and sorted")
        previous_host_only = entry["bundle_name"]
        if not sorted_names(entry["bundle_sets"], False):
            fail("verified ownership host-only bundle sets are not unique and sorted")
        if not sorted_names(entry["host_features"], False):
            fail("verified ownership host-only features are not unique and sorted")
    records, seen_binaries, previous_bundle = [], set(), None
    owner_names = set()
    for bundle in bundles:
        if not isinstance(bundle, dict) or set(bundle) != {"bundle_name", "bundle_sets", "host_features", "payloads"}:
            fail("verified ownership bundle has an invalid shape")
        bundle_name, sets, payloads = bundle["bundle_name"], bundle["bundle_sets"], bundle["payloads"]
        if (not isinstance(bundle_name, str) or not name.fullmatch(bundle_name)
                or previous_bundle is not None and bundle_name <= previous_bundle):
            fail("verified ownership bundle names are not unique and sorted")
        previous_bundle = bundle_name
        owner_names.add(bundle_name)
        if not sorted_names(sets, False):
            fail("verified ownership bundle sets are not unique and sorted")
        if not sorted_names(bundle["host_features"], True):
            fail("verified ownership bundle host features are not unique and sorted")
        if not isinstance(payloads, list) or not payloads:
            fail("data-only bundles must be absent from ownership")
        previous_binary = None
        for payload in payloads:
            if (not isinstance(payload, dict)
                    or set(payload) != {"binary", "cargo_package", "build_class", "cargo_features"}):
                fail("verified owned payload has an invalid shape")
            binary, package, build_class = payload["binary"], payload["cargo_package"], payload["build_class"]
            if (not isinstance(binary, str) or not name.fullmatch(binary)
                    or previous_binary is not None and binary <= previous_binary
                    or binary in seen_binaries):
                fail("verified owned binaries are not globally unique and sorted")
            if (not isinstance(package, str) or not name.fullmatch(package)
                    or build_class not in {"release", "static"}):
                fail("verified owned payload has invalid package or class")
            if package in ownership["host_packages"]:
                fail("verified owned payload belongs to a host package")
            if package in ownership["no_default_features_packages"] and not payload["cargo_features"]:
                fail("verified owned payload of a no-default-features package lacks an explicit feature composition")
            if not sorted_names(payload["cargo_features"], True):
                fail("verified owned payload features are not unique and sorted")
            previous_binary = binary
            seen_binaries.add(binary)
            records.append({"bundle":bundle_name,"binary":binary,"cargo_package":package,
                            "build_class":build_class,"bundle_sets":sets,
                            "required_features":payload["cargo_features"]})
            if len(records) > 256:
                fail("verified owned payloads exceed their bound")
    lists = [set(ownership["data_only_bundles"]), {entry["bundle_name"] for entry in host_only}, owner_names]
    if (len(lists[0]) + len(lists[1]) + len(lists[2])
            != len(lists[0] | lists[1] | lists[2])):
        fail("verified ownership bundle lists are not disjoint")
    return records, hashlib.sha256(canonical.encode("utf-8")).hexdigest(), implications, ownership["no_default_features_packages"]

# This helper is part of the authenticated Tool source closure, not project code.
elf_spec = importlib.util.spec_from_file_location(
    "release_elf", pathlib.Path(__file__).with_name("release-elf.py"))
release_elf = importlib.util.module_from_spec(elf_spec)
elf_spec.loader.exec_module(release_elf)
request = json.load(sys.stdin)
if not isinstance(request, dict) or set(request) != {"release_input", "resolved_config"}:
    fail("closed build request and verified ownership resolution required")
value = request["release_input"]
required = {"schema","project_path","bundle_name","authored_manifest","source_snapshot_hash","predecessor_generation_hash",
            "target","build_profile","payload_ownership_item_ref","payload_ownership_content_hash",
            "payloads","cargo_packages","cargo_feature_selections",
            "build_classes","requires_binary_build","clean_output_required","ambient_target_reuse_allowed"}
if not isinstance(value, dict) or set(value) != required: fail("release input shape changed")
if value["schema"] != "ryeos.bundle_release_input_plan.v2": fail("unsupported release input schema")
if value["build_profile"] != "release" or not value["clean_output_required"] or value["ambient_target_reuse_allowed"]:
    fail("clean release build is mandatory")
# Source reads are rooted in the admitted pinned execution generation. The
# original path remains provenance only and is never ambient build authority.
root = pathlib.Path.cwd().resolve(strict=True)
ownership_records, ownership_identity, ownership_implications, no_default_features_packages = verified_ownership_projection(request["resolved_config"])
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
selected_features = feature_selections(expected, ownership_implications)
classes = sorted({payload["build_class"] for payload in expected})
if (value["cargo_packages"] != packages
        or value["cargo_feature_selections"] != selected_features
        or value["build_classes"] != classes):
    fail("release input package, feature, or build-class projection is incorrect")
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
# The admitted CoW project hard-links verified content-cache inodes into its
# lower tree. A source file's link count therefore does not describe authored
# links; shutil.copytree creates a private output inode below.
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
            package_features = next(
                (selection["features"] for selection in selected_features
                 if selection["cargo_package"] == package),
                [],
            )
            command = [cargo, "--config", 'source.crates-io.replace-with="ryeos-vendored"',
                       "--config", 'source.ryeos-vendored.directory="/ryeos/realizations/cargo-vendor"',
                       "build", "--release", "--locked", "--frozen", "--offline",
                       "--jobs", "2", "--target", triple, "-p", package]
            if package in no_default_features_packages: command.append("--no-default-features")
            if package_features: command.extend(["--features", ",".join(package_features)])
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
