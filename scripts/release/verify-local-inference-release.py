#!/usr/bin/env python3
"""Verify the exact immutable local-inference realization asset set."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
from typing import Any


SCHEMA = "ryeos.local_inference_realization_release.v1"


def sha256_bytes(value: bytes) -> str:
    return hashlib.sha256(value).hexdigest()


def load_json(path: Path) -> dict[str, Any]:
    value = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(value, dict):
        raise ValueError(f"{path} must contain a JSON object")
    return value


def expected_assets(contract_path: Path) -> tuple[dict[str, str], str]:
    contract_bytes = contract_path.read_bytes()
    contract = json.loads(contract_bytes)
    if not isinstance(contract, dict) or contract.get("schema") != SCHEMA:
        raise ValueError("unsupported local-inference release contract")
    release_tag = contract.get("release_tag")
    if not isinstance(release_tag, str) or not release_tag:
        raise ValueError("release_tag must be a non-empty string")

    archive_digests: dict[str, str] = {}
    realization_components: list[str] = []
    for item in contract.get("realizations", []):
        if not isinstance(item, dict):
            raise ValueError("every realization must be an object")
        component = item.get("component")
        archive = item.get("archive")
        digest = item.get("sha256")
        url = item.get("url")
        if not all(isinstance(value, str) and value for value in (component, archive, digest, url)):
            raise ValueError("realization component/archive/sha256/url must be strings")
        if len(digest) != 64 or any(char not in "0123456789abcdef" for char in digest):
            raise ValueError(f"invalid archive digest for {archive}")
        expected_prefix = (
            f"https://github.com/leolilley/ryeos/releases/download/{release_tag}/"
        )
        if url != expected_prefix + archive:
            raise ValueError(f"non-canonical release URL for {archive}")
        if archive in archive_digests:
            raise ValueError(f"duplicate release archive {archive}")
        archive_digests[archive] = digest
        realization_components.append(component)
    if realization_components != ["runtime", "tinygrad", "toolchain", "model"]:
        raise ValueError("realization components are not the exact v1 set and order")

    for group in contract.get("corresponding_sources", []):
        if not isinstance(group, dict):
            raise ValueError("every corresponding-source group must be an object")
        for role in ("upstream", "packaging"):
            artifact = group.get(role)
            if not isinstance(artifact, dict):
                raise ValueError(f"corresponding-source {role} must be an object")
            archive = artifact.get("archive")
            digest = artifact.get("sha256")
            url = artifact.get("url")
            if not all(isinstance(value, str) and value for value in (archive, digest, url)):
                raise ValueError(f"corresponding-source {role} is incomplete")
            if len(digest) != 64 or any(char not in "0123456789abcdef" for char in digest):
                raise ValueError(f"invalid corresponding-source digest for {archive}")
            expected_prefix = (
                f"https://github.com/leolilley/ryeos/releases/download/{release_tag}/"
            )
            if url != expected_prefix + archive:
                raise ValueError(f"non-canonical release URL for {archive}")
            if archive in archive_digests:
                raise ValueError(f"duplicate release archive {archive}")
            archive_digests[archive] = digest

    assets = {
        "realizations.json": sha256_bytes(contract_bytes),
    }
    for archive, digest in archive_digests.items():
        assets[archive] = digest
        sidecar = f"{digest}  {archive}\n".encode()
        assets[f"{archive}.sha256"] = sha256_bytes(sidecar)
    return assets, release_tag


def verify_asset_directory(contract_path: Path, asset_dir: Path) -> None:
    expected, _ = expected_assets(contract_path)
    if not asset_dir.is_dir() or asset_dir.is_symlink():
        raise ValueError(f"asset directory is not an ordinary directory: {asset_dir}")
    entries = list(asset_dir.iterdir())
    observed_names = {entry.name for entry in entries}
    if observed_names != set(expected):
        raise ValueError(
            "release asset set differs: "
            f"expected={sorted(expected)}, observed={sorted(observed_names)}"
        )
    for entry in entries:
        if entry.is_symlink() or not entry.is_file():
            raise ValueError(f"release asset is not an ordinary file: {entry.name}")
        observed = hashlib.file_digest(entry.open("rb"), "sha256").hexdigest()
        if observed != expected[entry.name]:
            raise ValueError(
                f"release asset digest differs for {entry.name}: "
                f"expected={expected[entry.name]}, observed={observed}"
            )


def verify_release_metadata(
    contract_path: Path,
    release_json_path: Path,
    require_promoted: bool,
    allow_draft: bool = False,
    allow_partial: bool = False,
) -> None:
    if require_promoted and (allow_draft or allow_partial):
        raise ValueError("promoted-release verification cannot allow draft/partial state")
    if allow_partial and not allow_draft:
        raise ValueError("partial release state is allowed only for a private draft")
    expected, release_tag = expected_assets(contract_path)
    release = load_json(release_json_path)
    if release.get("tag_name") != release_tag:
        raise ValueError("release metadata names a different immutable tag")
    if not allow_draft and release.get("draft") is not False:
        raise ValueError("local-inference realization release is a draft")
    if allow_partial and release.get("draft") is not True:
        raise ValueError("partial local-inference release state is not private")
    if require_promoted and release.get("prerelease") is not False:
        raise ValueError("local-inference realization release is not promoted")

    assets = release.get("assets")
    if not isinstance(assets, list):
        raise ValueError("release metadata does not contain an asset list")
    by_name: dict[str, dict[str, Any]] = {}
    for asset in assets:
        if not isinstance(asset, dict) or not isinstance(asset.get("name"), str):
            raise ValueError("release metadata contains a malformed asset")
        name = asset["name"]
        if name in by_name:
            raise ValueError(f"release metadata contains duplicate asset {name}")
        by_name[name] = asset
    observed_names = set(by_name)
    expected_names = set(expected)
    names_match = observed_names <= expected_names if allow_partial else observed_names == expected_names
    if not names_match:
        raise ValueError(
            "release asset set differs: "
            f"expected={sorted(expected_names)}, observed={sorted(observed_names)}"
        )
    for name, asset in by_name.items():
        digest = expected[name]
        if asset.get("state") != "uploaded":
            raise ValueError(f"release asset is not fully uploaded: {name}")
        if asset.get("digest") != f"sha256:{digest}":
            raise ValueError(f"release asset digest differs: {name}")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--contract", type=Path, required=True)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--asset-dir", type=Path)
    source.add_argument("--release-json", type=Path)
    parser.add_argument("--require-promoted", action="store_true")
    parser.add_argument("--allow-draft", action="store_true")
    parser.add_argument("--allow-partial", action="store_true")
    args = parser.parse_args()

    try:
        if args.asset_dir is not None:
            if args.require_promoted:
                raise ValueError("--require-promoted applies only to release metadata")
            verify_asset_directory(args.contract, args.asset_dir)
        else:
            verify_release_metadata(
                args.contract,
                args.release_json,
                args.require_promoted,
                args.allow_draft,
                args.allow_partial,
            )
    except (OSError, ValueError, json.JSONDecodeError) as error:
        parser.error(str(error))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
