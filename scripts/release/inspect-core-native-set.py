#!/usr/bin/env python3
"""Record or compare a node's complete native artifact set.

Reads Core's `.ai/bin/**` binaries with their `*.item_source.json` sidecars,
the Core bundle manifests, the explicitly named daemon/CLI/handler binaries and
optionally one Python interpreter (resolved executable hash plus version).
Emits one canonical JSON record whose `digest` is the sha256 over the record
without that field. Tool YAML or scripts outside `bin/` never enter the record.

Symlinks and unexpected entries are refused by name; no link is ever followed
out of the root. The inspector is read-only.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import stat
import subprocess
import sys

sys.dont_write_bytecode = True

SCHEMA = "ryeos.core_native_set.v1"
SIDECAR = ".item_source.json"
SIGNATURE_PREFIX = b"# ryeos:signed:"
MANIFESTS = ("manifest.source.yaml", "manifest.yaml")


def fail(message):
    raise ValueError(message)


def sha256_file(path):
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def regular_file(path, label):
    info = os.lstat(path)
    if stat.S_ISLNK(info.st_mode):
        fail(f"{label} is a symlink: {path.name}")
    if not stat.S_ISREG(info.st_mode):
        fail(f"{label} is not a regular file: {path.name}")


def real_dir(path, label):
    try:
        info = os.lstat(path)
    except FileNotFoundError:
        fail(f"{label} is missing")
    if stat.S_ISLNK(info.st_mode):
        fail(f"{label} is a symlink")
    if not stat.S_ISDIR(info.st_mode):
        fail(f"{label} is not a directory")


def core_root(root):
    root = Path(os.path.abspath(root))
    real_dir(root, "root")
    nested = root / "bundles" / "core"
    if os.path.lexists(nested):
        real_dir(root / "bundles", "bundles")
        real_dir(nested, "bundles/core")
        return nested
    if os.path.lexists(root / ".ai"):
        return root
    fail("root holds neither bundles/core nor .ai")


def normalized_manifest_hash(path):
    # The leading signature line changes on every re-sign without changing
    # what the manifest declares.
    data = path.read_bytes()
    if data.startswith(SIGNATURE_PREFIX):
        newline = data.find(b"\n")
        data = b"" if newline < 0 else data[newline + 1:]
    return hashlib.sha256(data).hexdigest()


def walk_bin(bin_dir):
    files = []

    def visit(directory, prefix):
        for name in sorted(os.listdir(directory)):
            path = directory / name
            rel = f"{prefix}{name}"
            info = os.lstat(path)
            if stat.S_ISLNK(info.st_mode):
                fail(f"bin entry is a symlink: {rel}")
            if stat.S_ISDIR(info.st_mode):
                visit(path, rel + "/")
            elif stat.S_ISREG(info.st_mode):
                files.append(rel)
            else:
                fail(f"bin entry is not a regular file: {rel}")

    visit(bin_dir, "")
    return files


def interpreter_record(path):
    resolved = Path(os.path.realpath(path))
    if not resolved.is_file():
        fail(f"interpreter does not resolve to a file: {Path(path).name}")
    try:
        out = subprocess.run([str(resolved), "--version"], capture_output=True,
                             text=True, timeout=30, check=True)
    except (OSError, subprocess.SubprocessError) as error:
        fail(f"interpreter version probe failed: {error}")
    return {
        "name": resolved.name,
        "sha256": sha256_file(resolved),
        "version": (out.stdout or out.stderr).strip(),
    }


def build_record(root, binaries=(), interpreter=None):
    core = core_root(root)
    ai = core / ".ai"
    real_dir(ai, ".ai")
    manifests = {}
    for name in MANIFESTS:
        path = ai / name
        if os.path.lexists(path):
            regular_file(path, "manifest")
            manifests[name] = normalized_manifest_hash(path)
    if "manifest.yaml" not in manifests:
        fail("manifest.yaml is missing")

    bin_dir = ai / "bin"
    real_dir(bin_dir, ".ai/bin")
    names = walk_bin(bin_dir)
    sidecars = {n for n in names if n.endswith(SIDECAR)}
    payloads = {n for n in names if n not in sidecars}
    for payload in sorted(payloads):
        if payload + SIDECAR not in sidecars:
            fail(f"binary without item_source sidecar: {payload}")
    for sidecar in sorted(sidecars):
        if sidecar[:-len(SIDECAR)] not in payloads:
            fail(f"item_source sidecar without binary: {sidecar}")
    bins = {n: sha256_file(bin_dir / n) for n in sorted(payloads)}
    item_sources = {n: sha256_file(bin_dir / n) for n in sorted(sidecars)}

    selected = {}
    for spec in binaries:
        label, sep, raw = spec.partition("=")
        if not sep or not label or not raw:
            fail(f"binary selection must be LABEL=PATH: {spec}")
        if label in selected:
            fail(f"duplicate binary label: {label}")
        path = Path(os.path.abspath(raw))
        regular_file(path, f"selected binary {label}")
        selected[label] = {"name": path.name, "sha256": sha256_file(path)}

    record = {
        "schema": SCHEMA,
        "bin": bins,
        "item_sources": item_sources,
        "manifests": manifests,
        "selected_binaries": dict(sorted(selected.items())),
        "interpreter": interpreter_record(interpreter) if interpreter else None,
    }
    record["digest"] = digest_of(record)
    return record


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def digest_of(record):
    body = {k: v for k, v in record.items() if k != "digest"}
    return hashlib.sha256(canonical(body)).hexdigest()


def differences(old, new, path=""):
    found = []
    if isinstance(old, dict) and isinstance(new, dict):
        for key in sorted(set(old) | set(new)):
            here = f"{path}/{key}" if path else key
            if key not in new:
                found.append(f"removed: {here}")
            elif key not in old:
                found.append(f"added: {here}")
            else:
                found.extend(differences(old[key], new[key], here))
    elif old != new:
        found.append(f"changed: {path}")
    return found


def compare(recorded_path, current):
    recorded = json.loads(Path(recorded_path).read_text())
    if recorded.get("schema") != SCHEMA:
        fail("recorded file has an unknown schema")
    if recorded.get("digest") != digest_of(recorded):
        fail("recorded file digest does not match its content")
    return differences(
        {k: v for k, v in recorded.items() if k != "digest"},
        {k: v for k, v in current.items() if k != "digest"},
    )


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, required=True,
                        help="app root or bundle source root")
    parser.add_argument("--binary", action="append", default=[], metavar="LABEL=PATH",
                        help="daemon/CLI/handler binary (repeatable)")
    parser.add_argument("--interpreter", type=Path)
    parser.add_argument("--compare", type=Path, metavar="RECORD")
    args = parser.parse_args(argv)
    record = build_record(args.root, args.binary, args.interpreter)
    if args.compare:
        diffs = compare(args.compare, record)
        if diffs:
            for line in diffs:
                print(line)
            return 1
        print(f"identical: {record['digest']}")
        return 0
    sys.stdout.write(canonical(record).decode() + "\n")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError) as error:
        print(f"core native set inspection failed: {error}", file=sys.stderr)
        raise SystemExit(2)
