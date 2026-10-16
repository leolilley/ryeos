#!/usr/bin/env python3
"""Derive native build selections for a bundle set from signed payload ownership.

The publisher-signed ownership item declares, per bundle, the sets it belongs
to, the daemon-linked host features it contributes, and each native payload
with its Cargo package and per-package Cargo features. This tool computes the
`<package>/<feature>,...` selection that a set builds with. It uses only the
standard library: the first-line signature envelope is checked for body-hash
integrity here, while the Ed25519 signature itself is verified by
scripts/release/bundle-payload-ownership.py and the node.

Usage:
  python3 -I bundle-features.py [--repository-root DIR | --config FILE]
      (--set NAME | --bundles A,B,...) cargo-features PACKAGE

`cargo-features` prints one comma-joined line, or nothing when the package has
no selected feature. Set membership is known only for bundles that own native
payloads or contribute host features; every other bundle is listed as
data-only and has no effect on the selection.
"""

import argparse
import hashlib
import re
import sys
from pathlib import Path

OWNERSHIP_PATH = "bundles/bundle-release/.ai/config/bundle-release/payload-ownership.yaml"
SCHEMA = "ryeos.bundle_payload_ownership.v2"
HEADER = re.compile(r"# ryeos:signed:.+:([0-9a-f]{64}):([^:]+):([0-9a-f]{64})")


class OwnershipError(ValueError):
    pass


# ── Restricted YAML subset ───────────────────────────────────────────
# The ownership item uses block mappings, block sequences of mappings, flow
# lists of plain scalars, and plain or double-quoted scalars. Anything else is
# rejected rather than guessed at.

# Plain scalars that a YAML 1.1 or 1.2 reader would not take as strings.
_NON_STRING = re.compile(
    r"(?i)(y|n|yes|no|true|false|on|off|null|~|[-+]?\.(inf|nan)|[-+]?0[xob][0-9a-f_]+"
    r"|[-+]?(\d[\d_]*)?\.?\d[\d_]*([e][-+]?\d+)?|[-+]?\d[\d_]*(:[0-5]?\d)+(\.\d*)?"
    r"|\d{4}-\d\d?-\d\d?([ t].*)?)"
)


def _key_split(text):
    """Split `key: value` (or `key:` at line end); None when no mapping key is present."""
    for position, char in enumerate(text):
        if char == ":" and (position + 1 == len(text) or text[position + 1] == " "):
            return text[:position], text[position + 1:]
        if char in " #\"[{":
            return None
    return None


def _scalar(text):
    text = text.strip()
    if text.startswith('"'):
        if len(text) < 2 or not text.endswith('"') or '"' in text[1:-1] or "\\" in text:
            raise OwnershipError(f"unsupported quoted scalar: {text!r}")
        return text[1:-1]
    if not text or text[0] in "&*!|>{'%@`" or text.startswith("- ") or ":" in text or " #" in text:
        raise OwnershipError(f"unsupported scalar: {text!r}")
    if _NON_STRING.fullmatch(text):
        raise OwnershipError(f"plain scalar {text!r} is not a string in YAML; quote it")
    return text


def _value(text):
    text = text.strip()
    if text.startswith("["):
        if not text.endswith("]"):
            raise OwnershipError(f"unterminated flow list: {text!r}")
        inner = text[1:-1].strip()
        return [] if not inner else [_scalar(part) for part in inner.split(",")]
    return _scalar(text)


def _lines(body):
    result = []
    for number, raw in enumerate(body.splitlines(), 1):
        if "\t" in raw:
            raise OwnershipError(f"line {number}: tabs are not allowed")
        stripped = raw.strip()
        if not stripped or stripped.startswith("#"):
            continue
        result.append((len(raw) - len(raw.lstrip(" ")), stripped, number))
    return result


def _parse_block(lines, index, indent):
    """Parse the block starting at lines[index] whose indentation is `indent`."""
    if lines[index][1].startswith("- "):
        items = []
        while index < len(lines) and lines[index][0] == indent and lines[index][1].startswith("- "):
            _, text, number = lines[index]
            rest = text[2:].strip()
            if not rest.startswith(("[", '"')) and _key_split(rest) is not None:
                # A mapping item: its first key sits on the dash line.
                lines[index] = (indent + 2, rest, number)
                item, index = _parse_block(lines, index, indent + 2)
                items.append(item)
            else:
                items.append(_value(rest))
                index += 1
        return items, index
    mapping = {}
    while index < len(lines) and lines[index][0] == indent and not lines[index][1].startswith("- "):
        _, text, number = lines[index]
        split = _key_split(text)
        if split is None or not split[0]:
            raise OwnershipError(f"line {number}: expected `key: value`")
        key, rest = split
        if key in mapping:
            raise OwnershipError(f"line {number}: duplicate field {key!r}")
        index += 1
        if rest.strip():
            mapping[key] = _value(rest)
        elif index < len(lines) and (lines[index][0] > indent or (
                lines[index][0] == indent and lines[index][1].startswith("- "))):
            mapping[key], index = _parse_block(lines, index, lines[index][0])
        else:
            raise OwnershipError(f"line {number}: field {key!r} has no value")
    if index < len(lines) and lines[index][0] > indent:
        raise OwnershipError(f"line {lines[index][2]}: unexpected indentation")
    return mapping, index


def parse_yaml(body):
    lines = _lines(body)
    if not lines or lines[0][0] != 0:
        raise OwnershipError("empty or indented document")
    document, index = _parse_block(lines, 0, 0)
    if index != len(lines):
        raise OwnershipError(f"line {lines[index][2]}: unexpected content")
    return document


# ── Ownership document ───────────────────────────────────────────────

def load_ownership(path):
    raw = Path(path).read_bytes().decode("utf-8")
    first, _, body = raw.partition("\n")
    match = HEADER.fullmatch(first)
    if not match:
        raise OwnershipError("ownership config requires a first-line signature envelope")
    if hashlib.sha256(body.encode("utf-8")).hexdigest() != match.group(1):
        raise OwnershipError("ownership config body hash does not match its signature envelope")
    ownership = parse_yaml(body).get("payload_ownership")
    if not isinstance(ownership, dict) or ownership.get("schema") != SCHEMA:
        raise OwnershipError(f"payload ownership schema must be {SCHEMA}")
    for field in ("host_packages", "no_default_features_packages", "feature_implications", "data_only_bundles", "host_only_bundles", "bundles"):
        if field not in ownership:
            raise OwnershipError(f"payload ownership lacks {field}")
    for field in ("host_packages", "no_default_features_packages"):
        names = ownership[field]
        if not isinstance(names, list) or names != sorted(set(names)) or not all(
                isinstance(n, str) and re.fullmatch(r"[a-z0-9_-]{1,64}", n) for n in names):
            raise OwnershipError(f"{field} must be a unique sorted list of package names")
    seen_bundles = set()
    seen_binaries = set()
    for bundle in ownership["host_only_bundles"] + ownership["bundles"]:
        name = bundle["bundle_name"]
        if name in seen_bundles or name in ownership["data_only_bundles"]:
            raise OwnershipError(f"duplicate bundle ownership record {name!r}")
        seen_bundles.add(name)
        for payload in bundle.get("payloads", []):
            if payload["binary"] in seen_binaries:
                raise OwnershipError(f"binary {payload['binary']!r} has more than one owner")
            seen_binaries.add(payload["binary"])
            if payload["cargo_package"] in ownership["host_packages"]:
                raise OwnershipError(f"{payload['cargo_package']} is both a host package and a payload package")
            if payload["cargo_package"] in ownership["no_default_features_packages"] and not payload["cargo_features"]:
                raise OwnershipError(f"{payload['binary']}: {payload['cargo_package']} requires an explicit feature composition")
    return ownership


def _records(ownership):
    """Bundles with ownership records, host-only first, each group in file order."""
    return ownership["host_only_bundles"] + ownership["bundles"]


def set_members(ownership, set_name):
    """Ownership-recorded members of a set, by name; data-only members are not listed."""
    members = sorted(b["bundle_name"] for b in _records(ownership) if set_name in b["bundle_sets"])
    if not members:
        raise OwnershipError(f"unknown bundle set {set_name!r}")
    return members


def _closure(implications, selected):
    implied = set()
    pending = list(selected)
    while pending:
        for feature in implications.get(pending.pop(), []):
            if feature not in implied:
                implied.add(feature)
                pending.append(feature)
    return implied


def cargo_features(ownership, bundles, package):
    """Return the `package/feature` entries `bundles` build `package` with.

    `bundles` is ordered; host features follow that order, de-duplicated.
    Names listed as data-only contribute nothing; any other unknown name is an error.
    Payload features of a package are unioned across the bundles, then any
    feature implied by another selected feature is dropped, and the rest is
    sorted. Selected payloads of one package must agree on their build class.
    """
    if len(set(bundles)) != len(bundles):
        raise OwnershipError("bundle list contains a duplicate bundle")
    by_name = {b["bundle_name"]: b for b in _records(ownership)}
    for name in bundles:
        if name not in by_name and name not in ownership["data_only_bundles"]:
            raise OwnershipError(f"unknown bundle {name!r}")
    bundles = [name for name in bundles if name in by_name]
    selected = set()
    classes = set()
    for name in bundles:
        for payload in by_name[name].get("payloads", []):
            if payload["cargo_package"] == package:
                selected.update(payload["cargo_features"])
                classes.add(payload["build_class"])
    if len(classes) > 1:
        raise OwnershipError(f"{package}: payloads disagree on build class {sorted(classes)}")
    if package in ownership["host_packages"]:
        features = []
        for name in bundles:
            for feature in by_name[name]["host_features"]:
                if feature not in features:
                    features.append(feature)
    else:
        features = sorted(selected - _closure(ownership["feature_implications"].get(package, {}), selected))
    return [f"{package}/{feature}" for feature in features]


def derive(ownership, package, set_name=None, bundles=None):
    if (set_name is None) == (bundles is None):
        raise OwnershipError("select exactly one of a set or a bundle list")
    members = set_members(ownership, set_name) if set_name is not None else bundles
    return cargo_features(ownership, members, package)


def render(entries):
    return ",".join(entries) + "\n" if entries else ""


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--repository-root", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("--config", type=Path)
    group = parser.add_mutually_exclusive_group(required=True)
    group.add_argument("--set", dest="set_name")
    group.add_argument("--bundles")
    sub = parser.add_subparsers(dest="query", required=True)
    sub.add_parser("cargo-features").add_argument("package")
    args = parser.parse_args(argv)
    ownership = load_ownership(args.config or args.repository_root / OWNERSHIP_PATH)
    bundles = None if args.bundles is None else [b for b in args.bundles.split(",") if b]
    sys.stdout.write(render(derive(ownership, args.package, args.set_name, bundles)))


if __name__ == "__main__":
    try:
        main()
    except OwnershipError as error:
        print(f"bundle features rejected: {error}", file=sys.stderr)
        raise SystemExit(1)
