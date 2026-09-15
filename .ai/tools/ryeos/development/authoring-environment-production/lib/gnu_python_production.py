# ryeos:signed:2026-09-08T15:46:29Z:00ae05ae023f66fca9e41806e6748cff858e45be2ff64369a678a095ee63e66f:omP05JMSF6IzMnxauHuBOolsau8Pl1g6MsJvER5aM65z6/zf9iOhMU38tLNpwfofgq6vf9NiVmrzGKEjzOmeDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Finite GNU CPython distribution production from admitted immutable inputs.

The Tool owns one exact archive-tree pin; its enclosing Graph supplies one
selected prepared-input product. This module performs no acquisition, host
discovery, Python execution, publication, or qualification. It selects and
checks bytes, calls the existing admitted ELF tools, and writes only its
private workspace output.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import sys

from archives import read_members
from gnu_elf import (canonical_inventory, inspect_python_runtime,
                     inventory_evidence, relocate_python_runtime)
from gnu_python_archives import (Limits, checked_archive, extract_install,
                                 extract_metadata)
from production import ElfTools, canonical_json, ordinary_member, relative, sha256


SCHEMA = "ryeos.gnu_python_production_inputs.v2"
CONFIG_REF = "development/ryeos/gnu-python-production-inputs.yaml"
ARCHIVE_ROOT = Path("/ryeos/realizations/gnu-python-archives")
SUPPORT_ROOT = Path("/ryeos/realizations/gnu-python-support")
OUTPUT = PurePosixPath("products/gnu-python-distribution")
MAX_CONFIG_BYTES = 256 * 1024
MAX_ARCHIVE_ENTRIES = 8
MAX_METADATA_FILES = 256
MAX_SUPPORT_FILES = 32
HASH = re.compile(r"[0-9a-f]{64}")


def _identity(value: object, *, keys=frozenset({"bytes", "sha256"}), exact=True) -> dict:
    if (not isinstance(value, dict) or not keys.issubset(value) or
            (exact and set(value) != keys) or
            type(value["bytes"]) is not int or not 0 <= value["bytes"] <= 1024**3 or
            not isinstance(value["sha256"], str) or not HASH.fullmatch(value["sha256"])):
        raise ValueError("invalid exact file identity")
    if "mode" in keys and value["mode"] not in (0o644, 0o755):
        raise ValueError("invalid exact file mode")
    return value


def _archive(value: object) -> dict:
    keys = {"member", "artifact", "bytes", "sha256", "asset_id", "url"}
    _identity(value, keys=frozenset({"bytes", "sha256"}), exact=False)
    if (set(value) != keys or len(relative(value["member"]).parts) != 1 or
            not isinstance(value["artifact"], str) or not value["artifact"] or
            type(value["asset_id"]) is not int or value["asset_id"] <= 0 or
            not isinstance(value["url"], str) or not value["url"].startswith("https://github.com/") or
            len(value["url"]) > 2048):
        raise ValueError("invalid exact upstream archive coordinate")
    return value


def validate_contract(config: object) -> dict:
    required = {
        "category", "version", "description", "schema", "source_date_epoch",
        "runtime", "archives", "metadata_files", "compatibility_files",
        "retained_gnu_sources", "needed_replacements", "needed_additions", "license_supplements",
        "qualification_requirements",
    }
    if (not isinstance(config, dict) or set(config) != required or
            config["category"] != "development/ryeos" or config["version"] != "1.0.0" or
            config["schema"] != SCHEMA or not isinstance(config["description"], str) or
            type(config["source_date_epoch"]) is not int or
            not 0 <= config["source_date_epoch"] <= 4_102_444_800):
        raise ValueError("unsupported or incomplete GNU Python production contract")
    expected_runtime = {
        "version": "3.14.7", "major_minor": "3.14", "cache_tag": "cpython-314",
        "soabi": "cpython-314-x86_64-linux-gnu",
        "target_triple": "x86_64-unknown-linux-gnu", "platform_tag": "linux-x86_64",
        "build_options": "pgo+lto", "libpython_link_mode": "shared",
        "crt_features": ["glibc-dynamic", "glibc-max-symbol-version:2.17)"],
        "runtime_root": "/ryeos/realizations/python-gnu/python",
    }
    if config["runtime"] != expected_runtime:
        raise ValueError("GNU Python runtime metadata differs from the admitted build")
    archives = config["archives"]
    if not isinstance(archives, dict) or set(archives) != {"manifest", "install", "metadata"}:
        raise ValueError("exact GNU Python archive set is missing")
    manifest = archives["manifest"]
    if (manifest != {"digest": "ec7d4a2a749b0b709dabca0a7919c3a897a9b671be56332b67a659b6bd82cac6",
                     "storage": "large_content", "entries": 4}):
        raise ValueError("GNU Python archive manifest identity differs")
    install, metadata = _archive(archives["install"]), _archive(archives["metadata"])
    if (install["member"] != "cpython-gnu-install.tar.gz" or
            metadata["member"] != "cpython-gnu-full.tar.zst"):
        raise ValueError("GNU Python archive aliases differ from the admitted manifest")
    metadata_files = config["metadata_files"]
    if (not isinstance(metadata_files, dict) or not 1 <= len(metadata_files) <= MAX_METADATA_FILES or
            list(metadata_files) != sorted(metadata_files) or "PYTHON.json" not in metadata_files):
        raise ValueError("GNU Python metadata inventory is not canonical")
    for name, identity in metadata_files.items():
        relative(name)
        _identity(identity)
    for field in ("compatibility_files", "retained_gnu_sources"):
        values = config[field]
        if (not isinstance(values, dict) or not 1 <= len(values) <= MAX_SUPPORT_FILES or
                list(values) != sorted(values)):
            raise ValueError(f"{field} is not a finite canonical selection")
        for target, identity in values.items():
            relative(target)
            if not isinstance(identity, dict) or set(identity) != {"source", "bytes", "mode", "sha256"}:
                raise ValueError(f"{field} has an incomplete identity")
            relative(identity["source"])
            _identity(identity, keys=frozenset({"bytes", "mode", "sha256"}), exact=False)
    replacements = config["needed_replacements"]
    if not isinstance(replacements, dict) or list(replacements) != sorted(replacements):
        raise ValueError("DT_NEEDED replacement declarations are not canonical")
    for member, mapping in replacements.items():
        relative(member)
        if (not isinstance(mapping, dict) or not mapping or list(mapping) != sorted(mapping) or
                any(not isinstance(old, str) or "/" not in old or
                    not isinstance(new, str) or not re.fullmatch(r"[A-Za-z0-9_+.-]+", new)
                    for old, new in mapping.items())):
            raise ValueError("invalid finite DT_NEEDED replacement declaration")
    additions = config["needed_additions"]
    if (not isinstance(additions, dict) or not additions or
            list(additions) != sorted(additions)):
        raise ValueError("DT_NEEDED addition declarations are not canonical")
    for member, names in additions.items():
        relative(member)
        if (not isinstance(names, list) or not names or len(names) > 16 or
                names != sorted(set(names)) or
                any(not isinstance(name, str) or
                    not re.fullmatch(r"[A-Za-z0-9_+.-]+", name) for name in names)):
            raise ValueError("invalid finite DT_NEEDED addition declaration")
    supplement = config["license_supplements"]
    if not isinstance(supplement, dict) or set(supplement) != {"zstd"}:
        raise ValueError("exact zstd source/license supplement is missing")
    zstd = supplement["zstd"]
    if (not isinstance(zstd, dict) or
            set(zstd) != {"member", "bytes", "sha256", "url", "selected_members"} or
            zstd["member"] != "cpython-source-deps-zstd-1.5.7.tar.gz"):
        raise ValueError("invalid exact zstd supplement")
    _identity(zstd, keys=frozenset({"bytes", "sha256"}), exact=False)
    if not isinstance(zstd["url"], str) or not zstd["url"].startswith("https://github.com/"):
        raise ValueError("invalid zstd source URL testimony")
    selected = zstd["selected_members"]
    if (not isinstance(selected, dict) or list(selected) != sorted(selected) or len(selected) != 2):
        raise ValueError("zstd license selection is not exact")
    for member, identity in selected.items():
        relative(member)
        _identity(identity)
    requirements = config["qualification_requirements"]
    if not isinstance(requirements, dict) or set(requirements) != {"zlib-ng"}:
        raise ValueError("zlib qualification requirement must be explicit")
    exception = requirements["zlib-ng"]
    if (not isinstance(exception, dict) or
            set(exception) != {"missing_metadata_member", "upstream_build",
                               "expected_runtime_zlib_version"} or
            exception["missing_metadata_member"] != "licenses/LICENSE.zlib-ng.txt" or
            exception["expected_runtime_zlib_version"] != "1.3.2"):
        raise ValueError("invalid zlib qualification requirement")
    build = exception["upstream_build"]
    if not isinstance(build, dict) or set(build) != {"member", "bytes", "sha256"}:
        raise ValueError("zlib qualification lacks the exact upstream build recipe")
    _identity(build, exact=False)
    if build["member"] != "upstream-build-zlib.sh":
        raise ValueError("zlib qualification names an unexpected build recipe")
    return config


def _check_regular(path: Path, identity: dict, *, executable: bool | None = None) -> Path:
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_size != identity["bytes"] or sha256(path) != identity["sha256"]:
        raise ValueError(f"exact input identity mismatch: {path.name}")
    if executable is not None and bool(info.st_mode & 0o111) != executable:
        raise ValueError(f"input executable class mismatch: {path.name}")
    return path


def _check_archive_tree(root: Path, config: dict) -> dict[str, Path]:
    if not stat.S_ISDIR(root.lstat().st_mode):
        raise ValueError("GNU Python archive realization is not an ordinary directory")
    expected = {
        config["archives"]["install"]["member"]: config["archives"]["install"],
        config["archives"]["metadata"]["member"]: config["archives"]["metadata"],
        config["license_supplements"]["zstd"]["member"]: config["license_supplements"]["zstd"],
        config["qualification_requirements"]["zlib-ng"]["upstream_build"]["member"]:
            config["qualification_requirements"]["zlib-ng"]["upstream_build"],
    }
    children = sorted(root.iterdir(), key=lambda path: path.name)
    if len(children) > MAX_ARCHIVE_ENTRIES or [path.name for path in children] != sorted(expected):
        raise ValueError("GNU Python archive realization differs from its four-entry manifest")
    return {name: _check_regular(root / name, identity, executable=False)
            for name, identity in expected.items()}


def _metadata_inventory(root: Path) -> dict:
    result = {}
    for directory, subdirs, files in os.walk(root, followlinks=False):
        subdirs.sort()
        files.sort()
        for name in subdirs:
            if (Path(directory) / name).is_symlink():
                raise ValueError("metadata license tree contains a symlink")
        for name in files:
            path = Path(directory) / name
            if path.is_symlink() or not path.is_file():
                raise ValueError("metadata license tree contains a non-regular file")
            member = path.relative_to(root).as_posix()
            result[member] = {"bytes": path.stat().st_size, "sha256": sha256(path)}
            if len(result) > MAX_METADATA_FILES:
                raise ValueError("metadata license inventory exceeds its bound")
    return result


def _copy_support(support: Path, destination: Path, selections: dict) -> None:
    for target, identity in selections.items():
        source = ordinary_member(support, identity["source"])
        _check_regular(source, identity, executable=identity["mode"] == 0o755)
        output = destination.joinpath(*relative(target).parts)
        output.parent.mkdir(parents=True, exist_ok=True, mode=0o755)
        with source.open("rb") as input_stream, output.open("xb") as output_stream:
            shutil.copyfileobj(input_stream, output_stream, length=1024 * 1024)
        output.chmod(identity["mode"])
        if sha256(output) != identity["sha256"]:
            raise ValueError("copied support file changed")


def produce(project: Path, config: dict) -> dict:
    validate_contract(config)
    archive_files = _check_archive_tree(ARCHIVE_ROOT, config)
    ordinary_member(SUPPORT_ROOT, "elf", directory=True)
    parent = project / "products"
    parent.mkdir(mode=0o700, exist_ok=True)
    destination = project.joinpath(*OUTPUT.parts)
    # The process workspace is private until terminal capture, so the exact
    # admitted output root is also the staging root. Keeping every transient
    # mutation below it prevents a failed build from escaping the declared
    # output partition.
    staging = destination
    if destination.exists() or destination.is_symlink():
        raise ValueError("GNU Python production requires a fresh workspace")
    staging.mkdir(mode=0o700)
    install_dir, metadata_dir = staging / "install-selection", staging / "metadata-selection"
    install = config["archives"]["install"]
    metadata = config["archives"]["metadata"]
    install_receipt = extract_install(archive_files[install["member"]], install_dir,
                                      expected_sha256=install["sha256"],
                                      expected_bytes=install["bytes"],
                                      limits=Limits(entries=10000))
    metadata_receipt, python_metadata = extract_metadata(
        archive_files[metadata["member"]], metadata_dir,
        expected_sha256=metadata["sha256"], expected_bytes=metadata["bytes"])
    metadata_root = metadata_dir / "python"
    if _metadata_inventory(metadata_root) != config["metadata_files"]:
        raise ValueError("GNU Python upstream metadata/license inventory differs")
    runtime = config["runtime"]
    expected_python = {
        "python_version": runtime["version"],
        "python_major_minor_version": runtime["major_minor"],
        "python_tag": "cp314", "python_abi_tag": "",
        "python_implementation_cache_tag": runtime["cache_tag"],
        "python_platform_tag": runtime["platform_tag"],
        "target_triple": runtime["target_triple"],
        "build_options": runtime["build_options"],
        "libpython_link_mode": runtime["libpython_link_mode"],
        "crt_features": runtime["crt_features"],
        "python_extension_module_loading": ["builtin", "shared-library"],
        "python_exe": "install/bin/python3.14",
        "license_path": "licenses/LICENSE.cpython.txt",
    }
    if {key: python_metadata.get(key) for key in expected_python} != expected_python:
        raise ValueError("PYTHON.json differs from the exact admitted GNU build metadata")
    runtime_output = staging / "python"
    shutil.move(install_dir / "python", runtime_output)
    install_dir.rmdir()
    notices = staging / "notices"
    notices.mkdir(mode=0o755)
    shutil.move(metadata_root, notices / "python-build-standalone")
    metadata_dir.rmdir()
    zstd = config["license_supplements"]["zstd"]
    selected_names = set(zstd["selected_members"])
    with checked_archive(archive_files[zstd["member"]], zstd["sha256"], zstd["bytes"]) as stream:
        selected = read_members(stream, selected_names, maximum_member_bytes=64 * 1024,
                                maximum_selected_bytes=128 * 1024)
    zstd_notices = notices / "zstd"
    zstd_notices.mkdir(mode=0o755)
    for member, data in sorted(selected.items()):
        identity = zstd["selected_members"][member]
        if len(data) != identity["bytes"] or hashlib.sha256(data).hexdigest() != identity["sha256"]:
            raise ValueError("selected zstd license bytes differ")
        notice = zstd_notices / Path(member).name
        notice.write_bytes(data)
        notice.chmod(0o644)
    build = config["qualification_requirements"]["zlib-ng"]["upstream_build"]
    zlib_notices = notices / "zlib"
    zlib_notices.mkdir(mode=0o755)
    zlib_recipe = zlib_notices / build["member"]
    shutil.copyfile(archive_files[build["member"]], zlib_recipe)
    zlib_recipe.chmod(0o644)
    _copy_support(SUPPORT_ROOT, runtime_output, config["compatibility_files"])
    _copy_support(SUPPORT_ROOT, notices / "gnu-runtime", config["retained_gnu_sources"])
    tools = ElfTools(SUPPORT_ROOT)
    relocations = relocate_python_runtime(
        runtime_output, tools, runtime_root=runtime["runtime_root"],
        needed_replacements=config["needed_replacements"],
        needed_additions=config["needed_additions"],
        python_version=runtime["major_minor"], soabi=runtime["soabi"])
    elf_inventory = inspect_python_runtime(
        runtime_output, tools, runtime_root=runtime["runtime_root"],
        python_version=runtime["major_minor"], soabi=runtime["soabi"])
    inventory_bytes = canonical_inventory(elf_inventory)
    inventory_path = staging / "elf-inventory.json"
    inventory_path.write_bytes(inventory_bytes)
    inventory_path.chmod(0o644)
    relocation_bytes = canonical_json(relocations)
    relocations_path = staging / "relocations.json"
    relocations_path.write_bytes(relocation_bytes)
    relocations_path.chmod(0o644)
    evidence = {
        "schema": "ryeos.gnu-python-production-evidence.v1",
        "config_sha256": hashlib.sha256(canonical_json(config)).hexdigest(),
        "archive_manifest_digest": config["archives"]["manifest"]["digest"],
        "install_receipt": install_receipt,
        "metadata_receipt": metadata_receipt,
        "elf": inventory_evidence(elf_inventory),
        "relocations_sha256": hashlib.sha256(relocation_bytes).hexdigest(),
        "runtime_executed": False,
    }
    evidence_path = staging / "production-evidence.json"
    evidence_path.write_bytes(canonical_json(evidence))
    evidence_path.chmod(0o644)
    epoch = config["source_date_epoch"]
    for directory, _, files in os.walk(staging, followlinks=False):
        for name in files:
            path = Path(directory) / name
            if not path.is_symlink():
                os.utime(path, (epoch, epoch), follow_symlinks=False)
        os.utime(directory, (epoch, epoch), follow_symlinks=False)
    return {"output_path": OUTPUT.as_posix(), "evidence": evidence,
            "binding_published": False}


def run_production() -> None:
    if len(sys.argv) != 3 or sys.argv[1] != "--project-path":
        raise ValueError("missing admitted project context")
    project = Path(sys.argv[2])
    if not project.is_absolute() or not stat.S_ISDIR(project.lstat().st_mode):
        raise ValueError("invalid admitted project context")
    raw = sys.stdin.buffer.read(MAX_CONFIG_BYTES + 1)
    if len(raw) > MAX_CONFIG_BYTES:
        raise ValueError("GNU Python production parameters exceed their bound")
    request = json.loads(raw)
    resolved = request.get("resolved_config") if isinstance(request, dict) else None
    if not isinstance(resolved, dict):
        raise ValueError("missing exact GNU Python production Config")
    # The signed declaration uses ConfigResolveSpec::Single. Its exact runtime
    # contract injects the resolved Config body directly; only `multi` produces
    # a path-keyed map. validate_contract closes the admitted body below.
    result = produce(project, resolved)
    print(json.dumps({"ok": True, **result}, sort_keys=True))
