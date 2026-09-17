#!/usr/bin/env python3
"""Materialize one exact reviewed local-inference model source tree.

This publisher-side utility acquires or copies only files named by a reviewed
source contract. It does not create a RyeOS realization, worker, provider,
activation declaration, target binding, or qualification claim.
"""

from __future__ import annotations

import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import stat
import sys
import tempfile
from typing import Any
import urllib.request


SCHEMA = "ryeos.local_inference_model_source.v1"
CONTRACT_NAME = "MODEL_SOURCE.json"
MAX_FILES = 32
MAX_METADATA_BYTES = 16 * 1024 * 1024
MAX_SELECTED_BYTES = 16 * 1024 * 1024 * 1024
NETWORK_INACTIVITY_TIMEOUT_SECONDS = 60
DOWNLOAD_PROGRESS_BYTES = 512 * 1024 * 1024
_HEX_40 = re.compile(r"[0-9a-f]{40}")
_HEX_64 = re.compile(r"[0-9a-f]{64}")
_MODEL_ID = re.compile(r"[a-z0-9][a-z0-9.-]{0,63}")
_REPOSITORY_COMPONENT = r"[A-Za-z0-9](?:[A-Za-z0-9._-]{0,62}[A-Za-z0-9])?"
_REPOSITORY = re.compile(rf"{_REPOSITORY_COMPONENT}/{_REPOSITORY_COMPONENT}")
_FILE_NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,254}")
_ROLES = {
    "model_config",
    "generation_config",
    "weight_index",
    "weight_shard",
    "tokenizer",
    "tokenizer_config",
    "license_evidence",
}


def _strict_object(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, member in pairs:
        if key in value:
            raise ValueError(f"duplicate JSON object member: {key}")
        value[key] = member
    return value


def _reject_constant(value: str) -> None:
    raise ValueError(f"non-finite JSON number is forbidden: {value}")


def sha256_stream(source: io.BufferedIOBase) -> str:
    digest = hashlib.sha256()
    for chunk in iter(lambda: source.read(1024 * 1024), b""):
        digest.update(chunk)
    return digest.hexdigest()


def open_verified(path: Path, item: dict[str, Any]) -> io.BufferedReader:
    try:
        declared = path.lstat()
    except OSError as error:
        raise ValueError(f"model source is not an ordinary file: {item['path']}") from error
    if not stat.S_ISREG(declared.st_mode):
        raise ValueError(f"model source is not an ordinary file: {item['path']}")
    flags = os.O_RDONLY | os.O_CLOEXEC | os.O_NONBLOCK
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(path, flags)
    except OSError as error:
        raise ValueError(f"model source is not an ordinary file: {item['path']}") from error
    try:
        observed = os.fstat(descriptor)
        if not stat.S_ISREG(observed.st_mode):
            raise ValueError(f"model source is not an ordinary file: {item['path']}")
        if (observed.st_dev, observed.st_ino) != (declared.st_dev, declared.st_ino):
            raise ValueError(f"model source changed while opening: {item['path']}")
        if observed.st_size != item["bytes"]:
            raise ValueError(f"model source byte count differs: {item['path']}")
        source = os.fdopen(descriptor, "rb")
        descriptor = -1
        if sha256_stream(source) != item["sha256"]:
            source.close()
            raise ValueError(f"model source digest differs: {item['path']}")
        source.seek(0)
        return source
    finally:
        if descriptor >= 0:
            os.close(descriptor)


def load_contract(path: Path) -> tuple[dict[str, Any], bytes]:
    encoded = path.read_bytes()
    if not encoded or len(encoded) > MAX_METADATA_BYTES:
        raise ValueError("model source contract is outside its byte bound")
    try:
        value = json.loads(
            encoded,
            object_pairs_hook=_strict_object,
            parse_constant=_reject_constant,
        )
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise ValueError("model source contract is not strict JSON") from error
    if not isinstance(value, dict) or set(value) != {
        "schema",
        "model_id",
        "repository",
        "revision",
        "source",
        "license",
        "tensor_payload_bytes",
        "selected_file_bytes",
        "files",
    }:
        raise ValueError("model source contract has an unsupported shape")
    if value["schema"] != SCHEMA:
        raise ValueError("unsupported model source contract schema")
    if not isinstance(value["model_id"], str) or not _MODEL_ID.fullmatch(value["model_id"]):
        raise ValueError("model_id is not canonical")
    if not isinstance(value["repository"], str) or not _REPOSITORY.fullmatch(value["repository"]):
        raise ValueError("repository is not canonical")
    if not isinstance(value["revision"], str) or not _HEX_40.fullmatch(value["revision"]):
        raise ValueError("revision must be an exact lowercase commit")
    expected_source = f"https://huggingface.co/{value['repository']}"
    if value["source"] != expected_source:
        raise ValueError("source does not match the exact repository")
    license_value = value["license"]
    if (
        not isinstance(license_value, dict)
        or set(license_value) != {"spdx", "evidence_path"}
        or not isinstance(license_value["spdx"], str)
        or not license_value["spdx"]
        or not isinstance(license_value["evidence_path"], str)
    ):
        raise ValueError("license evidence is malformed")
    for field in ("tensor_payload_bytes", "selected_file_bytes"):
        if type(value[field]) is not int or value[field] <= 0:
            raise ValueError(f"{field} must be a positive exact integer")
    if value["selected_file_bytes"] > MAX_SELECTED_BYTES:
        raise ValueError("selected model source exceeds the publisher byte bound")

    files = value["files"]
    if not isinstance(files, list) or not 1 <= len(files) <= MAX_FILES:
        raise ValueError("model source file set is outside its cardinality bound")
    names: set[str] = set()
    roles: list[str] = []
    selected_bytes = 0
    for item in files:
        if not isinstance(item, dict) or set(item) != {"path", "role", "bytes", "sha256"}:
            raise ValueError("model source file entry has an unsupported shape")
        name = item["path"]
        role = item["role"]
        size = item["bytes"]
        digest = item["sha256"]
        if not isinstance(name, str) or not _FILE_NAME.fullmatch(name) or name == CONTRACT_NAME:
            raise ValueError("model source path is not a canonical root file")
        if name in names:
            raise ValueError(f"duplicate model source path: {name}")
        if not isinstance(role, str) or role not in _ROLES:
            raise ValueError(f"unsupported model source role for {name}")
        if type(size) is not int or size <= 0 or size > MAX_SELECTED_BYTES:
            raise ValueError(f"invalid exact byte count for {name}")
        if not isinstance(digest, str) or not _HEX_64.fullmatch(digest):
            raise ValueError(f"invalid SHA-256 for {name}")
        names.add(name)
        roles.append(role)
        selected_bytes += size
    if selected_bytes != value["selected_file_bytes"]:
        raise ValueError("selected_file_bytes does not match the exact file set")
    required_singletons = {
        "model_config",
        "generation_config",
        "weight_index",
        "tokenizer",
        "tokenizer_config",
        "license_evidence",
    }
    if any(roles.count(role) != 1 for role in required_singletons):
        raise ValueError("model source singleton roles are incomplete or duplicated")
    if roles.count("weight_shard") < 1:
        raise ValueError("model source has no weight shard")
    shard_bytes = sum(
        item["bytes"] for item in files if item["role"] == "weight_shard"
    )
    maximum_header_overhead = roles.count("weight_shard") * (
        MAX_METADATA_BYTES + 8
    )
    if not (
        value["tensor_payload_bytes"] < shard_bytes
        and shard_bytes - value["tensor_payload_bytes"] <= maximum_header_overhead
    ):
        raise ValueError("tensor payload is incoherent with the exact weight shards")
    if license_value["evidence_path"] not in names:
        raise ValueError("license evidence path is absent from the selected files")
    evidence = next(item for item in files if item["path"] == license_value["evidence_path"])
    if evidence["role"] != "license_evidence":
        raise ValueError("license evidence path has the wrong role")
    return value, encoded


def exact_url(contract: dict[str, Any], name: str) -> str:
    return f"{contract['source']}/resolve/{contract['revision']}/{name}"


def validate_file(path: Path, item: dict[str, Any]) -> None:
    with open_verified(path, item):
        pass


def copy_durable(source: io.BufferedIOBase, destination: Path) -> None:
    with destination.open("xb") as output_file:
        shutil.copyfileobj(source, output_file, 1024 * 1024)
        output_file.flush()
        os.fchmod(output_file.fileno(), 0o644)
        os.fsync(output_file.fileno())


def write_durable(destination: Path, value: bytes) -> None:
    with destination.open("xb") as output_file:
        output_file.write(value)
        output_file.flush()
        os.fchmod(output_file.fileno(), 0o644)
        os.fsync(output_file.fileno())


def sync_directory(path: Path) -> None:
    descriptor = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(descriptor)
    finally:
        os.close(descriptor)


def obtain(cache: Path, contract: dict[str, Any], item: dict[str, Any], offline: bool) -> Path:
    destination = cache / f"{item['sha256']}-{item['path']}"
    if destination.exists():
        print(f"reuse {item['path']} from exact digest cache", file=sys.stderr)
        validate_file(destination, item)
        return destination
    if offline:
        raise ValueError(f"offline model authoring is missing {item['path']}")
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{item['sha256']}-{item['path']}.", suffix=".download", dir=cache
    )
    temporary = Path(temporary_name)
    request = urllib.request.Request(
        exact_url(contract, item["path"]),
        headers={"User-Agent": "RyeOS-local-inference-model-authoring/1"},
    )
    try:
        print(f"acquire {item['path']} from exact upstream revision", file=sys.stderr)
        with urllib.request.urlopen(
            request,
            timeout=NETWORK_INACTIVITY_TIMEOUT_SECONDS,
        ) as response, os.fdopen(descriptor, "wb") as output:
            descriptor = -1
            remaining = item["bytes"]
            next_progress = min(DOWNLOAD_PROGRESS_BYTES, item["bytes"])
            while remaining:
                chunk = response.read(min(1024 * 1024, remaining + 1))
                if not chunk or len(chunk) > remaining:
                    raise ValueError(f"downloaded model source size differs: {item['path']}")
                output.write(chunk)
                remaining -= len(chunk)
                acquired = item["bytes"] - remaining
                if acquired >= next_progress:
                    print(
                        f"acquired {item['path']}: {acquired}/{item['bytes']} bytes",
                        file=sys.stderr,
                    )
                    next_progress += DOWNLOAD_PROGRESS_BYTES
            if response.read(1):
                raise ValueError(f"downloaded model source size differs: {item['path']}")
            output.flush()
            os.fsync(output.fileno())
        validate_file(temporary, item)
        temporary.chmod(0o644)
        try:
            os.link(temporary, destination, follow_symlinks=False)
            validate_file(destination, item)
            sync_directory(cache)
        except FileExistsError:
            validate_file(destination, item)
    except BaseException:
        if descriptor >= 0:
            os.close(descriptor)
        temporary.unlink(missing_ok=True)
        raise
    temporary.unlink(missing_ok=True)
    return destination


def verify_payload_tree(
    root: Path,
    contract: dict[str, Any],
    *,
    expected_root_mode: int = 0o755,
) -> None:
    if root.is_symlink() or not root.is_dir():
        raise ValueError("model source tree is not an ordinary directory")
    if stat.S_IMODE(root.stat().st_mode) != expected_root_mode:
        raise ValueError("model source tree mode is not canonical")
    expected = {item["path"] for item in contract["files"]}
    observed = {entry.name for entry in root.iterdir()}
    if observed != expected:
        raise ValueError(
            f"model source tree differs: expected={sorted(expected)}, observed={sorted(observed)}"
        )
    for item in contract["files"]:
        path = root / item["path"]
        validate_file(path, item)
        if stat.S_IMODE(path.stat().st_mode) != 0o644:
            raise ValueError(f"model source mode differs: {item['path']}")


def verify_tree(
    root: Path,
    contract: dict[str, Any],
    contract_bytes: bytes,
    *,
    expected_root_mode: int = 0o755,
) -> None:
    if root.is_symlink() or not root.is_dir():
        raise ValueError("model source tree is not an ordinary directory")
    if stat.S_IMODE(root.stat().st_mode) != expected_root_mode:
        raise ValueError("model source tree mode is not canonical")
    expected = {item["path"] for item in contract["files"]} | {CONTRACT_NAME}
    observed = {entry.name for entry in root.iterdir()}
    if observed != expected:
        raise ValueError(
            f"model source tree differs: expected={sorted(expected)}, observed={sorted(observed)}"
        )
    for item in contract["files"]:
        path = root / item["path"]
        validate_file(path, item)
        if stat.S_IMODE(path.stat().st_mode) != 0o644:
            raise ValueError(f"model source mode differs: {item['path']}")
    retained_contract = root / CONTRACT_NAME
    if retained_contract.is_symlink() or not retained_contract.is_file():
        raise ValueError("retained model source contract is not an ordinary file")
    if stat.S_IMODE(retained_contract.stat().st_mode) != 0o644:
        raise ValueError("retained model source contract mode differs")
    if retained_contract.read_bytes() != contract_bytes:
        raise ValueError("retained model source contract bytes differ")


def author_tree(
    *,
    output: Path,
    cache: Path | None,
    source: Path | None,
    offline: bool,
    contract: dict[str, Any],
    contract_bytes: bytes,
) -> None:
    if source is not None:
        try:
            source_metadata = source.lstat()
        except OSError as error:
            raise ValueError("local model source is not an ordinary directory") from error
        if not stat.S_ISDIR(source_metadata.st_mode):
            raise ValueError("local model source is not an ordinary directory")
    output.parent.mkdir(parents=True, exist_ok=True)
    if cache is not None:
        cache.mkdir(parents=True, exist_ok=True)
    try:
        output.mkdir(mode=0o700)
    except FileExistsError as error:
        raise ValueError(f"refusing existing output path: {output}") from error
    marker_published = False
    try:
        for item in contract["files"]:
            if source is not None:
                candidate = source / item["path"]
            else:
                if cache is None:
                    raise ValueError("model authoring without --source requires --cache")
                candidate = obtain(cache, contract, item, offline)
            destination = output / item["path"]
            with open_verified(candidate, item) as held_source:
                copy_durable(held_source, destination)
        sync_directory(output)
        verify_payload_tree(
            output,
            contract,
            expected_root_mode=0o700,
        )
        output.chmod(0o755)
        sync_directory(output)
        verify_payload_tree(output, contract)
        retained_contract = output / CONTRACT_NAME
        write_durable(retained_contract, contract_bytes)
        marker_published = True
        sync_directory(output)
        sync_directory(output.parent)
    except BaseException:
        if marker_published:
            print(
                "model source marker was published but durability acknowledgement "
                "failed; run --verify-tree before use or cleanup",
                file=sys.stderr,
            )
        # Before marker publication, the exclusively claimed coordinate is
        # incomplete and must be cleaned up explicitly. Removing either state
        # here would reintroduce a pathname race.
        raise


def main() -> int:
    parser = argparse.ArgumentParser(
        description="materialize or verify one exact reviewed model-source tree",
        epilog=(
            "Online authoring needs space for both the digest cache and output; "
            "the Qwen3-4B contract requires about 16.2 GB total. Local --source "
            "authoring needs only the output copy."
        ),
    )
    parser.add_argument("--contract", type=Path, required=True, help="reviewed source contract")
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--output", type=Path, help="absent output coordinate to claim")
    mode.add_argument("--verify-tree", type=Path, help="completed tree to verify")
    acquisition = parser.add_mutually_exclusive_group()
    acquisition.add_argument(
        "--cache", type=Path, help="digest cache for online/offline acquisition"
    )
    acquisition.add_argument(
        "--source", type=Path, help="exact local source directory; no cache required"
    )
    parser.add_argument("--offline", action="store_true", help="forbid network acquisition")
    args = parser.parse_args()
    try:
        contract_path = args.contract.resolve(strict=True)
        contract, contract_bytes = load_contract(contract_path)
        if args.verify_tree is not None:
            if args.cache is not None or args.source is not None or args.offline:
                raise ValueError("tree verification does not accept authoring inputs")
            verify_root = args.verify_tree.absolute()
            verify_tree(verify_root, contract, contract_bytes)
        else:
            if args.source is None and args.cache is None:
                raise ValueError("model authoring without --source requires --cache")
            source = args.source.absolute() if args.source is not None else None
            if source is not None and (source.is_symlink() or not source.is_dir()):
                raise ValueError("local model source is not an ordinary directory")
            author_tree(
                output=args.output.absolute(),
                cache=args.cache.resolve(strict=False) if args.cache is not None else None,
                source=source,
                offline=args.offline,
                contract=contract,
                contract_bytes=contract_bytes,
            )
    except (OSError, ValueError, json.JSONDecodeError) as error:
        parser.error(str(error))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
