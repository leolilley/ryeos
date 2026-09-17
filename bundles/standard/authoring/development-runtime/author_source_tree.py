#!/usr/bin/env python3
"""Author the raw source-input tree consumed by the offline preparation Tool."""

from __future__ import annotations

import argparse
import io
import json
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile

import yaml

from author_tree import archive_tree, manifest, obtain, sha256


SCHEMA = "ryeos.publisher_authoring_source.v1"
MAX_IMAGE_PAYLOAD = 32 * 1024 * 1024


def source(name: str, value: dict) -> dict:
    if not isinstance(value, dict) or set(value) != {"name", "url", "bytes", "sha256"}:
        raise ValueError(f"invalid {name} acquisition source")
    if Path(value["name"]).name != value["name"]:
        raise ValueError(f"invalid {name} acquisition name")
    return {"target": value["name"], "mode": 0o644, **value}


def put(root: Path, name: str, data: bytes | Path, mode: int = 0o644) -> None:
    target = root / name
    if target.exists() or target.is_symlink():
        raise ValueError(f"duplicate publisher output: {name}")
    target.parent.mkdir(parents=True, exist_ok=True, mode=0o755)
    if isinstance(data, bytes):
        target.write_bytes(data)
    else:
        shutil.copyfile(data, target)
    target.chmod(mode)


def image_payload(image: str, members: dict, patchelf: Path, expected_package_hash: str) -> bytes:
    program = r'''
import hashlib, io, json, pathlib, stat, subprocess, sys, tarfile
members = json.loads(sys.argv[1])
with tarfile.open(fileobj=sys.stdout.buffer, mode="w|") as output:
    def emit(name, data, mode):
        item = tarfile.TarInfo(name); item.size = len(data); item.mode = mode
        output.addfile(item, io.BytesIO(data))
    for path, (target, mode) in sorted(members.items()):
        source = pathlib.Path(path)
        if not stat.S_ISREG(source.lstat().st_mode):
            raise ValueError("selected image member is not regular: " + path)
        emit(target, source.read_bytes(), mode)
    package = pathlib.Path("/input/patchelf.deb")
    if hashlib.sha256(package.read_bytes()).hexdigest() != sys.argv[2]:
        raise ValueError("patchelf package changed")
    payload = subprocess.check_output(["/usr/bin/ar", "p", str(package), "data.tar.xz"])
    with tarfile.open(fileobj=io.BytesIO(payload), mode="r:xz") as archive:
        for path, target, mode in [
            ("./usr/bin/patchelf", "elf/bin/patchelf", 0o755),
            ("./usr/share/doc/patchelf/copyright", "notices/patchelf-COPYRIGHT", 0o644),
        ]:
            item = archive.getmember(path)
            if not item.isfile(): raise ValueError("patchelf member is not regular")
            emit(target, archive.extractfile(item).read(), mode)
'''
    command = [
        "docker", "run", "--rm", "--pull=never", "--network=none", "--read-only",
        "--cap-drop=ALL", "--security-opt=no-new-privileges", "--pids-limit=32",
        "--memory=256m", "--mount",
        f"type=bind,src={patchelf.resolve()},dst=/input/patchelf.deb,readonly",
        "--entrypoint", "/usr/bin/python3", image, "-c", program,
        json.dumps(members, sort_keys=True), expected_package_hash,
    ]
    result = subprocess.run(command, check=True, stdout=subprocess.PIPE)
    if len(result.stdout) > MAX_IMAGE_PAYLOAD:
        raise ValueError("selected publisher-image payload exceeds its bound")
    return result.stdout


def author(acquisition_path: Path, input_contract_path: Path, cache: Path,
           output_directory: Path, offline: bool) -> dict:
    acquisition = yaml.safe_load(acquisition_path.read_bytes())
    contract = yaml.safe_load(input_contract_path.read_bytes())
    if not isinstance(acquisition, dict) or acquisition.get("schema") != SCHEMA:
        raise ValueError("unsupported authoring-source acquisition contract")
    provenance = contract.get("provenance") if isinstance(contract, dict) else None
    if not isinstance(provenance, dict):
        raise ValueError("missing authoring input provenance")
    cache.mkdir(parents=True, exist_ok=True)
    output_directory.mkdir(parents=True, exist_ok=True)
    bootstrap = [source("bootstrap archive", item) for item in acquisition["bootstrap_archives"]]
    workload = source("workload package", acquisition["workload_package"])
    patcher_spec = source("ELF authoring package", acquisition["elf_authoring_package"])
    expected_archives = {
        item["name"]: {"bytes": item["bytes"], "sha256": item["sha256"]}
        for item in bootstrap
    }
    if expected_archives != provenance["utility_bootstrap_archives"]:
        raise ValueError("bootstrap acquisition and admitted input contract disagree")
    if workload["sha256"] != provenance["workload_package_sha256"]:
        raise ValueError("workload acquisition and admitted input contract disagree")
    if patcher_spec["sha256"] != provenance["elf_authoring_package_sha256"]:
        raise ValueError("ELF acquisition and admitted input contract disagree")
    selected = {item["name"]: obtain(cache, item, offline) for item in bootstrap + [workload, patcher_spec]}
    upstreams = provenance["source_and_notice_inputs"]
    for item in upstreams:
        selected[item["archive"]] = obtain(cache, {
            "target": item["archive"], "url": item["url"], "bytes": item["bytes"],
            "sha256": item["sha256"], "mode": 0o644,
        }, offline)
    payload = image_payload(
        provenance["bootstrap_image"], provenance["runtime_image_members"],
        selected[patcher_spec["name"]], patcher_spec["sha256"],
    )
    with tempfile.TemporaryDirectory(dir=output_directory, prefix=".source-tree-") as temporary:
        tree = Path(temporary) / "tree"
        tree.mkdir(mode=0o700)
        for item in bootstrap:
            put(tree, "bootstrap/" + item["name"], selected[item["name"]])
        put(tree, "workload/selected-package.tar.gz", selected[workload["name"]])
        for item in upstreams:
            put(tree, "upstreams/" + item["archive"], selected[item["archive"]])
        expected_image_targets = {
            value[0] for value in provenance["runtime_image_members"].values()
        } | {"elf/bin/patchelf", "notices/patchelf-COPYRIGHT"}
        with tarfile.open(fileobj=io.BytesIO(payload), mode="r:") as archive:
            seen = set()
            for item in archive:
                if not item.isfile() or item.name not in expected_image_targets or item.name in seen:
                    raise ValueError("unexpected publisher-image selection")
                seen.add(item.name)
                put(tree, item.name, archive.extractfile(item).read(), item.mode)
            if seen != expected_image_targets:
                raise ValueError("incomplete publisher-image selection")
        artifact = acquisition["artifact"]
        observed, digest = manifest(tree, "large_content")
        if (digest != artifact["manifest_digest"] or
                observed["entry_count"] != artifact["entries"] or
                observed["total_bytes"] != artifact["total_bytes"]):
            raise ValueError(
                "authored raw source tree differs from its signed identity: "
                f"digest={digest} entries={observed['entry_count']} "
                f"total_bytes={observed['total_bytes']}"
            )
        output = output_directory / artifact["archive"]
        archive_tree(tree, output, artifact["prefix"], acquisition["source_date_epoch"])
    return {
        "schema": "ryeos.publisher_tree_artifact.v1",
        "release_tag": artifact["release_tag"], "archive": output.name,
        "archive_bytes": output.stat().st_size, "archive_sha256": sha256(output),
        "prefix": artifact["prefix"], "storage": "large_content",
        "manifest_digest": digest, "entries": observed["entry_count"],
        "total_bytes": observed["total_bytes"],
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--acquisition", type=Path, required=True)
    parser.add_argument("--input-contract", type=Path, required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    print(json.dumps(author(args.acquisition, args.input_contract, args.cache,
                            args.output, args.offline), sort_keys=True))


if __name__ == "__main__":
    main()
