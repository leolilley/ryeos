#!/usr/bin/env python3
"""Verify and project the signed RyeOS bundle payload-ownership config item."""

import argparse
import base64
import hashlib
import json
from pathlib import Path
import re
import sys
import tomllib

import yaml
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey

HEADER = re.compile(r"# ryeos:signed:.+:([0-9a-f]{64}):([^:]+):([0-9a-f]{64})")
NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9._-]{0,127}")
BUNDLE = re.compile(r"[a-z0-9]+(?:-[a-z0-9]+)*")
MAX_BUNDLES = 256
MAX_PAYLOADS = 256


class StrictLoader(yaml.SafeLoader):
    pass


def construct_mapping(loader, node, deep=False):
    result = {}
    for key_node, value_node in node.value:
        key = loader.construct_object(key_node, deep=deep)
        if key in result:
            raise ValueError(f"duplicate YAML field {key!r}")
        result[key] = loader.construct_object(value_node, deep=deep)
    return result


StrictLoader.add_constructor(yaml.resolver.BaseResolver.DEFAULT_MAPPING_TAG, construct_mapping)


def exact(value, keys, where):
    if not isinstance(value, dict) or set(value) != set(keys):
        raise ValueError(f"{where} must contain exactly {sorted(keys)!r}")


def load(repository_root: Path, config_path=None):
    path = config_path or repository_root / "bundles/bundle-release/.ai/config/bundle-release/payload-ownership.yaml"
    raw = path.read_bytes()
    lines = raw.splitlines(keepends=True)
    if not lines:
        raise ValueError("ownership config is empty")
    header = lines[0].decode("ascii").rstrip("\r\n")
    match = HEADER.fullmatch(header)
    if not match or sum(line.startswith(b"# ryeos:signed:") for line in lines) != 1:
        raise ValueError("ownership config requires exactly one valid first-line signature envelope")
    claimed_hash, encoded_signature, fingerprint = match.groups()
    body = b"".join(lines[1:])
    if hashlib.sha256(body).hexdigest() != claimed_hash:
        raise ValueError("ownership config body hash does not match its signature envelope")
    trust = tomllib.loads((repository_root / "bundles/.ai/PUBLISHER_TRUST.toml").read_text())
    exact(trust, {"public_key", "fingerprint", "owner"}, "publisher trust")
    if trust["fingerprint"] != fingerprint:
        raise ValueError("ownership signer is not the pinned bundle publisher")
    public_prefix = "ed25519:"
    if not isinstance(trust["public_key"], str) or not trust["public_key"].startswith(public_prefix):
        raise ValueError("publisher trust has no Ed25519 public key")
    public = base64.b64decode(trust["public_key"][len(public_prefix):], validate=True)
    signature = base64.b64decode(encoded_signature, validate=True)
    if len(public) != 32 or len(signature) != 64 or hashlib.sha256(public).hexdigest() != fingerprint:
        raise ValueError("ownership signature key material is invalid")
    Ed25519PublicKey.from_public_bytes(public).verify(signature, claimed_hash.encode("ascii"))

    document = yaml.load(body, Loader=StrictLoader)
    exact(document, {"category", "version", "description", "payload_ownership"}, "ownership config")
    if document["category"] != "bundle-release" or document["version"] != "1.0.0":
        raise ValueError("ownership config category or version is unsupported")
    if not isinstance(document["description"], str) or not document["description"]:
        raise ValueError("ownership config description is empty")
    ownership = document["payload_ownership"]
    exact(ownership, {"schema", "kind", "bundles"}, "payload_ownership")
    if ownership["schema"] != "ryeos.bundle_payload_ownership.v1" or ownership["kind"] != "bundle_payload_ownership":
        raise ValueError("payload ownership schema or kind is unsupported")
    bundles = ownership["bundles"]
    if not isinstance(bundles, list) or len(bundles) > MAX_BUNDLES:
        raise ValueError("payload ownership bundles exceed their bound")
    records = []
    seen_binaries = set()
    previous_bundle = None
    for bundle in bundles:
        exact(bundle, {"bundle_name", "bundle_sets", "payloads"}, "bundle ownership")
        name = bundle["bundle_name"]
        if not isinstance(name, str) or not BUNDLE.fullmatch(name) or previous_bundle is not None and name <= previous_bundle:
            raise ValueError("bundle ownership records must have unique canonical names in strict order")
        previous_bundle = name
        sets = bundle["bundle_sets"]
        if not isinstance(sets, list) or not sets or sets != sorted(set(sets)) or any(not isinstance(item, str) or not BUNDLE.fullmatch(item) for item in sets):
            raise ValueError(f"{name}: bundle_sets must be non-empty, unique, and sorted")
        payloads = bundle["payloads"]
        if not isinstance(payloads, list) or len(payloads) > MAX_PAYLOADS:
            raise ValueError(f"{name}: payloads exceed their bound")
        previous_binary = None
        for payload in payloads:
            exact(payload, {"binary", "cargo_package", "build_class"}, "owned payload")
            binary = payload["binary"]
            package = payload["cargo_package"]
            if not isinstance(binary, str) or not NAME.fullmatch(binary) or previous_binary is not None and binary <= previous_binary:
                raise ValueError(f"{name}: payload binaries must be unique and sorted")
            if binary in seen_binaries:
                raise ValueError(f"binary {binary!r} has more than one owner")
            if not isinstance(package, str) or not NAME.fullmatch(package) or payload["build_class"] not in {"release", "static"}:
                raise ValueError(f"{name}/{binary}: invalid package or build class")
            previous_binary = binary
            seen_binaries.add(binary)
            records.append({"bundle": name, "binary": binary, "cargo_package": package,
                            "build_class": payload["build_class"], "bundle_sets": sets})
    if len(records) > MAX_PAYLOADS:
        raise ValueError("payload ownership exceeds its global payload bound")
    identity = hashlib.sha256(json.dumps(ownership, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    return records, identity


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--repository-root", type=Path, required=True)
    parser.add_argument("--config", type=Path)
    parser.add_argument("--bundle")
    parser.add_argument("--bundle-set")
    parser.add_argument("--format", choices=["json", "records", "identity"], default="json")
    args = parser.parse_args()
    records, identity = load(args.repository_root.resolve(strict=True), args.config)
    if args.bundle is not None:
        records = [record for record in records if record["bundle"] == args.bundle]
    if args.bundle_set is not None:
        records = [record for record in records if args.bundle_set in record["bundle_sets"]]
    if args.format == "identity":
        print(identity)
    elif args.format == "records":
        for record in records:
            print("\t".join((record["bundle"], record["binary"], record["cargo_package"], record["build_class"])))
    else:
        json.dump({"identity": identity, "payloads": records}, sys.stdout, sort_keys=True, separators=(",", ":"))
        print()


if __name__ == "__main__":
    try:
        main()
    except Exception as error:
        print(f"bundle payload ownership rejected: {error}", file=sys.stderr)
        raise SystemExit(1)
