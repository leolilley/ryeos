#!/usr/bin/env python3
"""Audit an exact populated bundle tree and emit deterministic JSON."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import stat
import sys

MAX_ENTRIES = 10_000
MAX_FILE_BYTES = 32 * 1024 * 1024
MAX_TOTAL_BYTES = 256 * 1024 * 1024
ALLOWED_FILE_MODES = {0o644, 0o755}
_ownership_spec = importlib.util.spec_from_file_location(
    "bundle_payload_ownership", Path(__file__).with_name("bundle-payload-ownership.py")
)
_ownership = importlib.util.module_from_spec(_ownership_spec)
_ownership_spec.loader.exec_module(_ownership)


def audit_bundle(path: Path):
    entries = files = directories = total = 0
    maximum = {"bytes": 0, "path": None}
    modes = set()
    violations = []
    for current, dirnames, filenames in os.walk(path, followlinks=False):
        dirnames.sort()
        filenames.sort()
        base = Path(current)
        for name in dirnames + filenames:
            entry = base / name
            relative = entry.relative_to(path).as_posix()
            info = entry.lstat()
            entries += 1
            if stat.S_ISLNK(info.st_mode):
                violations.append(f"{relative}: symbolic links are not allowed")
            elif stat.S_ISDIR(info.st_mode):
                directories += 1
            elif stat.S_ISREG(info.st_mode):
                files += 1
                total += info.st_size
                mode = stat.S_IMODE(info.st_mode)
                modes.add(mode)
                if mode not in ALLOWED_FILE_MODES:
                    violations.append(f"{relative}: unsupported file mode {mode:04o}")
                if info.st_nlink != 1:
                    violations.append(f"{relative}: multiply-linked file")
                if info.st_size > MAX_FILE_BYTES:
                    violations.append(f"{relative}: exceeds {MAX_FILE_BYTES}-byte file bound")
                if info.st_size > maximum["bytes"]:
                    maximum = {"bytes": info.st_size, "path": relative}
            else:
                violations.append(f"{relative}: special files are not allowed")
    if entries > MAX_ENTRIES:
        violations.append(f"tree has {entries} entries; bound is {MAX_ENTRIES}")
    if total > MAX_TOTAL_BYTES:
        violations.append(f"tree has {total} file bytes; bound is {MAX_TOTAL_BYTES}")
    return {
        "directories": directories,
        "entries": entries,
        "files": files,
        "file_bytes": total,
        "file_modes": [f"{mode:04o}" for mode in sorted(modes)],
        "largest_file": maximum,
        "violations": sorted(violations),
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", required=True, type=Path)
    parser.add_argument("--triple", required=True)
    parser.add_argument("--repository-root", type=Path, default=Path(__file__).parents[2])
    parser.add_argument("--ownership-config", type=Path)
    parser.add_argument("--require-populated", action="store_true")
    parser.add_argument("bundles", nargs="+")
    args = parser.parse_args()
    records, _identity = _ownership.load(args.repository_root.resolve(strict=True), args.ownership_config)
    report = {"schema": 1, "limits": {
        "max_entries": MAX_ENTRIES,
        "max_file_bytes": MAX_FILE_BYTES,
        "max_total_bytes": MAX_TOTAL_BYTES,
    }, "triple": args.triple, "bundles": {}, "violations": []}
    for bundle in sorted(set(args.bundles)):
        path = args.root / bundle
        if not path.is_dir() or path.is_symlink():
            report["violations"].append(f"{bundle}: missing or unsafe bundle root")
            continue
        report["bundles"][bundle] = audit_bundle(path)
        report["violations"].extend(
            f"{bundle}/{item}" for item in report["bundles"][bundle]["violations"]
        )
    if args.require_populated:
        declared = {}
        for record in records:
            bundle, binary = record["bundle"], record["binary"]
            declared.setdefault(bundle, set()).add(binary)
            if bundle not in args.bundles:
                continue
            target = args.root / bundle / ".ai/bin" / args.triple / binary
            if not target.is_file() or target.is_symlink() or not os.access(target, os.X_OK):
                report["violations"].append(
                    f"{bundle}: missing populated executable .ai/bin/{args.triple}/{binary}"
                )
        for bundle in args.bundles:
            bin_root = args.root / bundle / ".ai/bin" / args.triple
            if not bin_root.is_dir():
                continue
            for target in sorted(bin_root.iterdir()):
                if target.is_file() and os.access(target, os.X_OK) and target.name not in declared.get(bundle, set()):
                    report["violations"].append(
                        f"{bundle}: undeclared populated executable .ai/bin/{args.triple}/{target.name}"
                    )
    report["violations"].sort()
    json.dump(report, sys.stdout, sort_keys=True, separators=(",", ":"))
    sys.stdout.write("\n")
    return 1 if report["violations"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
