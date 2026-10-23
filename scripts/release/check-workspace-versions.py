#!/usr/bin/env python3
"""Check crate versions against the release version.

Usage: check-workspace-versions.py VERSION --manifests FILE [--root DIR]

FILE lists, one path per line (`#` comments allowed), every manifest that
carries the release version; each must equal VERSION. Its version is read
from [package] or, for a pyproject.toml, [project]. Every other workspace
member must not carry a release-grammar version (0.X.Y with X >= 5), so a
crate bumped without being listed is caught. `version.workspace = true`
resolves to [workspace.package] version.
"""
import argparse
import glob
import os
import re
import sys
import tomllib

RELEASE_GRAMMAR = re.compile(r"^0\.([0-9]+)\.[0-9]+(-.+)?$")


def load(path):
    with open(path, "rb") as f:
        return tomllib.load(f)


def read_manifests(path):
    out = []
    with open(path) as f:
        for raw in f:
            line = raw.split("#", 1)[0].strip()
            if line:
                out.append(os.path.normpath(line))
    return out


def member_dirs(root, ws):
    excluded = {os.path.normpath(e) for e in ws.get("exclude", [])}
    dirs = []
    for pattern in ws.get("members", []):
        for hit in sorted(glob.glob(os.path.join(root, pattern))):
            rel = os.path.normpath(os.path.relpath(hit, root))
            if rel in excluded or not os.path.isfile(os.path.join(hit, "Cargo.toml")):
                continue
            dirs.append(rel)
    return dirs


def manifest_version(doc, ws_version):
    table = doc.get("package") or doc.get("project") or {}
    found = table.get("version")
    if isinstance(found, dict) and found.get("workspace") is True:
        found = ws_version
    return table.get("name"), found


def check(root, version, manifests):
    ws = load(os.path.join(root, "Cargo.toml")).get("workspace", {})
    ws_version = ws.get("package", {}).get("version")
    problems = []
    listed = set(manifests)
    for rel in manifests:
        full = os.path.join(root, rel)
        if not os.path.isfile(full):
            problems.append((rel, None, f"listed manifest is missing"))
            continue
        name, found = manifest_version(load(full), ws_version)
        if found != version:
            problems.append((rel, found, f"listed manifest must be {version}"))
    for d in member_dirs(root, ws):
        rel = os.path.join(d, "Cargo.toml")
        if rel in listed:
            continue
        name, found = manifest_version(load(os.path.join(root, rel)), ws_version)
        m = RELEASE_GRAMMAR.match(found) if isinstance(found, str) else None
        if m and int(m.group(1)) >= 5:
            problems.append((rel, found, "unlisted crate carries a release-grammar version"))
    return problems


def main(argv):
    ap = argparse.ArgumentParser()
    ap.add_argument("version")
    ap.add_argument("--manifests", required=True)
    ap.add_argument("--root", default=".")
    args = ap.parse_args(argv)
    bad = check(args.root, args.version, read_manifests(args.manifests))
    for rel, found, why in bad:
        print(f"::error::{rel} has version {found!r}: {why}", file=sys.stderr)
    if bad:
        return 1
    print(f"all listed manifests are at {args.version}; no unlisted release-version crates")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
