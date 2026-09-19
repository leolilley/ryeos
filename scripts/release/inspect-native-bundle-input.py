#!/usr/bin/env python3
"""Emit a bounded, read-only plan for one native bundle generation.

This inspector resolves payload ownership from the single machine-readable
contract. It never discovers or reuses ambient build output and performs no
build, signing, publication, or filesystem mutation.
"""

import argparse
import importlib.util
import json
from pathlib import Path
import re
import sys
import yaml

sys.dont_write_bytecode = True

HASH = re.compile(r"[0-9a-f]{64}")
NAME = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")
TRIPLE = re.compile(r"[A-Za-z0-9_]+(?:-[A-Za-z0-9_.]+)+")
_ownership_spec = importlib.util.spec_from_file_location(
    "bundle_payload_ownership", Path(__file__).with_name("bundle-payload-ownership.py")
)
_ownership = importlib.util.module_from_spec(_ownership_spec)
_ownership_spec.loader.exec_module(_ownership)


def fail(message):
    raise ValueError(message)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository-root", type=Path, required=True)
    parser.add_argument("--bundle", required=True)
    parser.add_argument("--source-snapshot-hash", required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--build-profile", choices=["release"], default="release")
    parser.add_argument("--predecessor-generation-hash")
    parser.add_argument("--ownership-config", type=Path)
    args = parser.parse_args()

    root = args.repository_root.resolve(strict=True)
    if not NAME.fullmatch(args.bundle):
        fail("bundle name is not canonical")
    if args.bundle == "core":
        fail("core is substrate-owned and cannot use bundle-only publication")
    if not HASH.fullmatch(args.source_snapshot_hash):
        fail("source snapshot hash must be a lowercase SHA-256 identity")
    if args.predecessor_generation_hash is not None and not HASH.fullmatch(
        args.predecessor_generation_hash
    ):
        fail("predecessor generation hash must be a lowercase SHA-256 identity")
    if args.target != "portable" and not TRIPLE.fullmatch(args.target):
        fail("target must be 'portable' or an exact target triple")
    bundle_root = root / "bundles" / args.bundle
    if not bundle_root.is_dir() or bundle_root.is_symlink():
        fail("bundle source root is missing or unsafe")

    all_records, ownership_identity = _ownership.load(root, args.ownership_config)
    records = [record for record in all_records if record["bundle"] == args.bundle]
    records.sort(key=lambda record: record["binary"])
    if records and args.target == "portable":
        fail("a bundle with native payload ownership requires an exact target triple")
    if not records and args.target != "portable":
        fail("a data-only bundle requires the portable target")
    packages = sorted({record["cargo_package"] for record in records})
    build_classes = sorted({record["build_class"] for record in records})
    target = {"kind": "portable"} if args.target == "portable" else {
        "kind": "triple", "triple": args.target
    }
    # Offline preview only: authenticated inspection uses RyeOS's typed source
    # manifest generator and rechecks its exact result before build dispatch.
    authored_manifest = yaml.safe_load((bundle_root / ".ai/manifest.source.yaml").read_text())
    if authored_manifest.get("name") != args.bundle:
        fail("manifest source names another bundle")
    for key, default in (("description", ""), ("requires_kinds", []), ("uses_kinds", [])):
        authored_manifest.setdefault(key, default)
    kinds = bundle_root / ".ai/node/engine/kinds"
    authored_manifest["provides_kinds"] = sorted(
        entry.name for entry in kinds.iterdir()
        if entry.is_dir() and (entry / f"{entry.name}.kind-schema.yaml").is_file()
    ) if kinds.is_dir() else []
    output = {
        "schema": "ryeos.bundle_release_input_plan.v1",
        "project_path": str(root),
        "bundle_name": args.bundle,
        "authored_manifest": authored_manifest,
        "source_snapshot_hash": args.source_snapshot_hash,
        "predecessor_generation_hash": args.predecessor_generation_hash,
        "target": target,
        "build_profile": args.build_profile,
        "payload_ownership_item_ref": "config:bundle-release/payload-ownership",
        "payload_ownership_content_hash": ownership_identity,
        "payloads": records,
        "cargo_packages": packages,
        "build_classes": build_classes,
        "requires_binary_build": bool(records),
        "clean_output_required": True,
        "ambient_target_reuse_allowed": False,
    }
    json.dump(output, sys.stdout, sort_keys=True, separators=(",", ":"))
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError) as error:
        print(f"native bundle input inspection failed: {error}", file=sys.stderr)
        raise SystemExit(1)
