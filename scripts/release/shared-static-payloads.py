#!/usr/bin/env python3
"""Stage and verify static payloads that several bundles carry from one build.

The signed ownership config gives every binary exactly one owning bundle and
one Cargo build. A binary that other bundles must also carry is declared in
shared-static-payloads.yaml with its owner and its consumers. Staging measures
the owner's built bytes once, installs the same bytes for every consumer and
records the single digest. Verification checks every published copy, and the
item-source blob each bundle's signed publication recorded, against that digest.
"""

import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import shutil
import sys

import yaml

SCHEMA = "ryeos.shared_static_payloads.v1"
RECORD = "shared-payloads.json"
NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}")
BUNDLE = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")
ADAPTER_ROLES = ("adapter", "supervisor", "launcher", "restoration_verifier")


class Refused(Exception):
    pass


def ownership_module(root: Path):
    path = root / "scripts/release/bundle-payload-ownership.py"
    spec = importlib.util.spec_from_file_location("bundle_payload_ownership", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def load_shared(root: Path, path=None):
    path = path or root / "scripts/release/shared-static-payloads.yaml"
    document = yaml.safe_load(path.read_text())
    if not isinstance(document, dict) or set(document) != {"schema", "description", "shared_payloads"}:
        raise Refused("shared payload config must contain exactly schema, description and shared_payloads")
    if document["schema"] != SCHEMA:
        raise Refused("shared payload schema is unsupported")
    return document["shared_payloads"]


def declared_binaries(root: Path):
    """(bundle, role, binary) for every executable a bundle manifest names."""
    found = []
    for manifest in sorted(root.glob("bundles/*/.ai/manifest.source.yaml")):
        bundle = manifest.parents[1].name
        document = yaml.safe_load(manifest.read_text()) or {}
        for adapter in document.get("external_lifecycle_adapters") or []:
            for role in ADAPTER_ROLES:
                if role in adapter:
                    found.append((bundle, role, adapter[role]))
    return found


def validate(records, shared, declared):
    """Refuse any shared declaration or manifest reference the ownership does not cover."""
    owner_of = {}
    for record in records:
        if record["binary"] in owner_of:
            raise Refused(f"payload {record['binary']} has more than one owner; shared payloads are built once")
        owner_of[record["binary"]] = record
    bundles_of = {}
    for record in records:
        bundles_of.setdefault(record["bundle"], set()).add(record["binary"])
    if not isinstance(shared, list):
        raise Refused("shared_payloads must be a list")
    consumers_of = {}
    seen = set()
    for entry in shared:
        if not isinstance(entry, dict) or set(entry) != {"binary", "owner_bundle", "consumer_bundles"}:
            raise Refused("shared payload entry must contain exactly binary, owner_bundle and consumer_bundles")
        binary, owner, consumers = entry["binary"], entry["owner_bundle"], entry["consumer_bundles"]
        if not isinstance(binary, str) or not NAME.fullmatch(binary) or binary in seen:
            raise Refused(f"shared payload {binary!r} is invalid or repeated")
        seen.add(binary)
        record = owner_of.get(binary)
        if record is None:
            raise Refused(f"shared payload {binary} has no owner in the signed ownership config")
        if record["bundle"] != owner:
            raise Refused(f"shared payload {binary} is owned by {record['bundle']}, not {owner}")
        if record["build_class"] != "static":
            raise Refused(f"shared payload {binary} must be a static build")
        if (not isinstance(consumers, list) or not consumers or consumers != sorted(set(consumers))
                or any(not isinstance(c, str) or not BUNDLE.fullmatch(c) for c in consumers)):
            raise Refused(f"shared payload {binary} consumers must be a non-empty unique sorted list")
        for consumer in consumers:
            if consumer == owner or binary in bundles_of.get(consumer, ()):
                raise Refused(f"shared payload {binary}: consumer {consumer} must not own a copy")
            consumer_sets = {s for r in records if r["bundle"] == consumer for s in r["bundle_sets"]}
            if not consumer_sets:
                raise Refused(f"shared payload {binary}: consumer {consumer} has no ownership record")
            if not consumer_sets <= set(record["bundle_sets"]):
                raise Refused(f"shared payload {binary}: {consumer} is built in sets that do not build its owner")
            consumers_of.setdefault(binary, set()).add(consumer)
    for bundle, role, binary in declared:
        if binary in bundles_of.get(bundle, ()):
            continue
        if bundle in consumers_of.get(binary, ()):
            continue
        raise Refused(f"{bundle} names {role} {binary} but neither owns nor declares it shared")
    return shared


def sha256_file(path: Path):
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def stage(shared, stage_dir: Path, selected_bundles):
    """Install each shared payload's single build for every selected consumer."""
    record = {}
    for entry in shared:
        consumers = [c for c in entry["consumer_bundles"] if c in selected_bundles]
        if not consumers:
            continue
        binary = entry["binary"]
        source = stage_dir / entry["owner_bundle"] / binary
        if not source.is_file():
            raise Refused(f"shared payload {binary} is not staged for its owner {entry['owner_bundle']}")
        digest = sha256_file(source)
        for consumer in consumers:
            target = stage_dir / consumer / binary
            if target.exists():
                raise Refused(f"shared payload {binary} is already staged for {consumer}; copies are not built per target")
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, target)
            target.chmod(0o755)
            if sha256_file(target) != digest:
                raise Refused(f"shared payload {binary} copy for {consumer} differs from the single build")
        record[binary] = {"sha256": digest, "owner": entry["owner_bundle"], "consumers": consumers}
    (stage_dir / RECORD).write_text(json.dumps(record, sort_keys=True, indent=2) + "\n")
    return record


def item_source_hash(path: Path):
    lines = path.read_text().splitlines()
    return json.loads("\n".join(l for l in lines if not l.startswith("# ryeos:signed:")))["content_blob_hash"]


def verify(root: Path, record, triple):
    """Every installed copy and its signed item-source blob carries the recorded digest."""
    for binary, info in sorted(record.items()):
        for bundle in [info["owner"], *info["consumers"]]:
            installed = root / "bundles" / bundle / ".ai/bin" / triple / binary
            if not installed.is_file():
                raise Refused(f"{bundle} has no installed {binary}")
            if sha256_file(installed) != info["sha256"]:
                raise Refused(f"{bundle} carries a {binary} that differs from the single build")
            source = installed.with_name(binary + ".item_source.json")
            if not source.is_file():
                raise Refused(f"{bundle} has no published item source for {binary}")
            if item_source_hash(source) != info["sha256"]:
                raise Refused(f"{bundle} published {binary} under a different hash than the single build")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("command", choices=["validate", "stage", "records", "verify"])
    parser.add_argument("--repository-root", type=Path, required=True)
    parser.add_argument("--bundle-set")
    parser.add_argument("--stage-dir", type=Path)
    parser.add_argument("--triple")
    args = parser.parse_args()
    root = args.repository_root.resolve(strict=True)
    module = ownership_module(root)
    records, _identity = module.load(root)
    shared = validate(records, load_shared(root), declared_binaries(root))
    if args.command == "validate":
        return
    if args.command == "verify":
        verify(root, json.loads((args.stage_dir / RECORD).read_text()), args.triple)
        return
    selected = {r["bundle"] for r in records if args.bundle_set in r["bundle_sets"]}
    if args.command == "records":
        for entry in shared:
            owner_selected = entry["owner_bundle"] in selected
            for consumer in entry["consumer_bundles"]:
                if consumer in selected:
                    if not owner_selected:
                        raise Refused(f"{consumer} is selected without {entry['owner_bundle']}, which builds {entry['binary']}")
                    print("\t".join((consumer, entry["binary"])))
        return
    stage(shared, args.stage_dir, selected)


if __name__ == "__main__":
    try:
        main()
    except Refused as error:
        print(f"shared static payloads rejected: {error}", file=sys.stderr)
        raise SystemExit(1)
    except Exception as error:
        print(f"shared static payloads rejected: {error}", file=sys.stderr)
        raise SystemExit(1)
