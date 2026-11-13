#!/usr/bin/env python3
# ryeos:signed:2026-11-12T23:59:42Z:cb59d8de273bf57fc61d7cd08b04158983269a648aa2daefb5d580e1f96899df:x6iyDrFeivDPCIWdnzbmA4WtFxFbOXpkylZ8cpZjjxrUzY+GIiE+KAEkCm3qmGttQ9IlMlcov7iCHOpjnjDsCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Report where a dev node's installed binaries and bundles differ from a worktree.

External development host tool; it grants no RyeOS authority and changes
nothing. A node runs what its last install or policy generation copied in,
not what the worktree holds: a populate or rebuild alone changes nothing on
the node. Rows:

  host binary   installed `<bin dir>/<name>` vs the worktree's built copy (sha256)
  daemon source the installed ryeosd's baked revision vs worktree HEAD, and the
                tracked source files changed since (committed or not)
  bundle        every file of each installed bundle vs `<worktree>/bundles/<name>`
                (payload rows are skipped when the worktree is not populated)

  dev_node_drift_check.py [--app-root DIR] [--bin-dir DIR] [--bundles a,b]
                          [--worktree DIR] [--config-file F] [--json]

Defaults come from `config:development/ryeos/dev-node`. Exit 0 when nothing
drifted, 1 when something did, 2 when it cannot check.
"""

from __future__ import annotations

import argparse
import fnmatch
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys

_spec = importlib.util.spec_from_file_location(
    "dev_host", Path(__file__).resolve().parents[2] / "dev-host/lib/dev_host.py")
dev_host = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(dev_host)
ToolError = dev_host.ToolError

LISTED = 8


def row(kind: str, name: str, ok: bool | None, detail: str) -> dict:
    """ok None is a note: neither drift nor a clean match."""
    return {"kind": kind, "name": name, "ok": ok, "detail": detail}


def _hash(path: Path) -> str | None:
    try:
        return dev_host.sha256_file(path)
    except FileNotFoundError:
        return None
    except PermissionError:
        return "unreadable"


def host_binary_rows(node) -> list[dict]:
    rows = []
    for name in node.host_binaries:
        installed, built = node.installed(name), node.built(name)
        have, want = _hash(installed), _hash(built)
        if have is None or want is None:
            missing = installed if have is None else built
            rows.append(row("host binary", name, None, f"{missing} does not exist"))
        elif "unreadable" in (have, want):
            rows.append(row("host binary", name, None, "cannot read one side"))
        else:
            rows.append(row("host binary", name, have == want,
                            f"installed {have[:12]}, worktree build {want[:12]}"))
    return rows


def _git(worktree: Path, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run(["git", *args], cwd=worktree, capture_output=True, text=True)


def daemon_source_rows(node) -> list[dict]:
    ryeosd = node.installed("ryeosd")
    if not ryeosd.exists():
        return [row("daemon source", "ryeosd", None, f"{ryeosd} does not exist")]
    try:
        info = dev_host.build_info(ryeosd)
    except ToolError as error:
        return [row("daemon source", "ryeosd", None, str(error))]
    revision = str(info.get("revision") or "")
    commit = revision.removesuffix("-dirty")
    head = _git(node.worktree, "rev-parse", "HEAD").stdout.strip()
    if not commit or _git(node.worktree, "cat-file", "-e", f"{commit}^{{commit}}").returncode != 0:
        return [row("daemon source", "ryeosd", None,
                    f"installed daemon revision {revision or 'unknown'} is not in this worktree's history")]
    changed = set(_git(node.worktree, "diff", "--name-only", commit, "HEAD", "--",
                       *node.source_paths).stdout.split())
    changed |= set(_git(node.worktree, "diff", "--name-only", "HEAD", "--", *node.source_paths).stdout.split())
    detail = (f"installed daemon built at {commit[:12]}{' (dirty)' if revision.endswith('-dirty') else ''}, "
              f"worktree HEAD {head[:12]}")
    if changed:
        listed = sorted(changed)
        detail += f"; {len(listed)} source file(s) changed since: " + ", ".join(listed[:LISTED])
    elif revision.endswith("-dirty"):
        return [row("daemon source", "ryeosd", None, detail + "; built from uncommitted changes")]
    return [row("daemon source", "ryeosd", not changed, detail)]


def _files(root: Path) -> dict[str, Path]:
    found = {}
    for directory, _, names in os.walk(root, onerror=_raise):
        for name in names:
            path = Path(directory) / name
            found[path.relative_to(root).as_posix()] = path
    return found


def _raise(error: OSError) -> None:
    raise error


def bundle_rows(node, app_root: Path, selected: list[str] | None) -> list[dict]:
    installed_root = app_root / ".ai/bundles"
    try:
        installed_names = sorted(p.name for p in installed_root.iterdir() if p.is_dir())
    except OSError as error:
        raise ToolError(f"cannot list {installed_root}: {error.strerror}") from error
    names = selected or installed_names
    rows = []
    bin_dir = f".ai/bin/{node.triple}/"
    for name in names:
        node_dir, source_dir = installed_root / name, node.worktree / "bundles" / name
        if not node_dir.is_dir():
            rows.append(row("bundle", name, False, "not installed on the node"))
            continue
        if not source_dir.is_dir():
            rows.append(row("bundle", name, None, "installed on the node only (not in this worktree)"))
            continue
        try:
            have, want = _files(node_dir), _files(source_dir)
        except OSError as error:
            rows.append(row("bundle", name, None, f"cannot read: {error}"))
            continue
        ignored = lambda rel: any(fnmatch.fnmatch(rel, g) for g in node.drift_ignore)
        populated = any(rel.startswith(bin_dir) for rel in want)
        skip = lambda rel: ignored(rel) or (not populated and rel.startswith(bin_dir))
        differ = [rel for rel in sorted(set(have) & set(want))
                  if not skip(rel) and _hash(have[rel]) != _hash(want[rel])]
        missing = [rel for rel in sorted(set(want) - set(have)) if not skip(rel)]
        extra = [rel for rel in sorted(set(have) - set(want)) if not skip(rel)]
        parts = []
        for label, items in (("differ", differ), ("missing on node", missing), ("node only", extra)):
            if items:
                parts.append(f"{len(items)} {label}: " + ", ".join(items[:LISTED])
                             + (" ..." if len(items) > LISTED else ""))
        note = "" if populated else " (worktree payloads not populated; .ai/bin not compared)"
        rows.append(row("bundle", name, not parts, ("; ".join(parts) or "matches") + note))
    return rows


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--app-root", type=Path)
    parser.add_argument("--bin-dir", type=Path)
    parser.add_argument("--bundles", help="comma-separated bundle names (default: every installed bundle)")
    parser.add_argument("--worktree")
    parser.add_argument("--config-file", type=Path)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args(argv)
    try:
        worktree = dev_host.worktree_argument(args.worktree)
        node = dev_host.load_dev_node(args.config_file, worktree)
        if args.bin_dir:
            node.installed_bin_dir = args.bin_dir.resolve()
        app_root = (args.app_root or node.app_root).resolve()
        selected = [b for b in (args.bundles or "").split(",") if b] or None
        rows = host_binary_rows(node) + daemon_source_rows(node) + bundle_rows(node, app_root, selected)
    except ToolError as error:
        print(f"dev-node-drift-check: {error}", file=sys.stderr)
        return 2
    drifted = any(r["ok"] is False for r in rows)
    if args.json:
        print(json.dumps({"ok": not drifted, "app_root": str(app_root), "worktree": str(worktree),
                          "rows": rows}, indent=2))
    else:
        for r in rows:
            mark = {True: "ok  ", False: "DIFF", None: "note"}[r["ok"]]
            print(f"{mark} {r['kind']} {r['name']}: {r['detail']}")
        print("NODE MATCHES WORKTREE" if not drifted else "NODE DRIFTED FROM WORKTREE")
    return 1 if drifted else 0


if __name__ == "__main__":
    sys.exit(main())
