#!/usr/bin/env python3
# ryeos:signed:2026-11-22T01:26:56Z:010e41f01d62c3f46660fda64bedae0e76c629245d0b5d7c137de7a5eb6b6675:JD3x7lcc3kRIwXKjushE5O5Vsk41klB8rCPW0Pa6xEcey7QKHaTvqXJVsc8+GXl23xM735bj71yX2TI4x783Dw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# Externally invoked host helper in the development bundle; lib/ is excluded
# from Tool resolution, so this is signed source, not an admitted Tool. It
# grants no RyeOS authority. Run it from a checkout of this repository.
# Decide whether a merge-gate CI run may use the UI-only gate (full on any doubt).
"""Decide whether a merge-gate run may use the UI-only gate.

Prints `mode=ui-only` or `mode=full` (the form a GitHub Actions output file
takes), followed by the changed paths and the reason on stderr-style lines
prefixed with `#`. The workflow appends stdout to its output file and the run
summary.

The result is `ui-only` only when the merge base with the base ref exists, at
least one path changed, and EVERY changed path is under a declared UI path.
Any doubt yields `full`: an unknown path, a missing merge base, an empty diff,
a git failure, an unexpected exception. The script never exits non-zero for
those, so a classification fault cannot fail the run or skip the full gate.

It also prints `test_packages=<json>`: the cargo packages whose tests the
merge gate must run for this branch (see `touched_packages`). A fault yields
`test_packages=ERROR`, which the workflow treats as a failure, never as "none".

  --base-ref REF   the ref the branch will merge into (default origin/next)
  --root DIR       repository to classify (default: this checkout)
  --rule RULE      `wide` (default): every package whose files changed, or
                   `narrow`: only packages whose test code changed
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path

# Prefixes (directories end in "/") whose changes cannot affect anything the
# skipped jobs check: the client crates, the UI daemon crate, the UI bundle and
# the UI development notes. A change under bundles/ryeos-ui/ still needs the
# branch's own bundle refresh before tests; BUNDLE_PREFIXES marks that case.
UI_PREFIXES = (
    "crates/clients/",
    "crates/daemon/ryeos-ui/",
    "bundles/ryeos-ui/",
    ".ai/knowledge/ryeos/development/ui-",
)
BUNDLE_PREFIXES = ("bundles/",)


def git(root: str, *args: str) -> bytes:
    return subprocess.run(
        ["git", "-C", root, *args], check=True, capture_output=True
    ).stdout


# Crates the UI-only gate's own job already tests; listing them again would
# only compile and run them twice.
UI_GATE_PACKAGES = ("ryeos-client-base", "ryeos-client-terminal", "ryeos-ui")

# The daemon package's `tests/` directory holds the end-to-end binaries that
# need populated bundle state and spawn real daemons. The wide rule runs that
# package's lib and bin tests only; the narrow rule runs just the touched
# top-level test binaries.
HEAVY_E2E_PACKAGES = ("ryeosd",)
LIB_AND_BIN_ARGS = ["--lib", "--bins", "--no-tests=pass"]


def read_package(manifest: Path) -> tuple[str | None, bool]:
    """Return (package name or None, whether the manifest declares a workspace)."""
    try:
        doc = tomllib.loads(manifest.read_text())
    except Exception:
        return None, False
    package = doc.get("package")
    name = package.get("name") if isinstance(package, dict) else None
    return (name if isinstance(name, str) else None), "workspace" in doc


def owning_package(root: Path, rel: str) -> tuple[str, str, str | None] | None:
    """Map a repository path to (package name, package dir, manifest or None).

    Walks up from the path to the nearest Cargo.toml that names a [package].
    The manifest is None for a member of the root workspace and otherwise the
    manifest of the separate workspace that owns the package (the Rune
    workspace), which cargo must be pointed at explicitly.
    """
    directory = (root / rel).parent
    while True:
        manifest = directory / "Cargo.toml"
        if manifest.is_file():
            name, _ = read_package(manifest)
            if name:
                break
        if directory == root or directory == directory.parent:
            return None
        directory = directory.parent
    package_dir = directory.relative_to(root).as_posix()
    # A workspace root other than the repository root owns its packages.
    walk = directory
    while walk != root:
        manifest = walk / "Cargo.toml"
        if manifest.is_file() and read_package(manifest)[1]:
            return name, package_dir, manifest.relative_to(root).as_posix()
        walk = walk.parent
    try:
        members = tomllib.loads((root / "Cargo.toml").read_text()).get("workspace", {}).get("members", [])
    except Exception:
        return None
    if package_dir != "." and package_dir not in members:
        return None  # not a root workspace member: `cargo test -p` could not select it
    return name, package_dir, None


def mask_rust(src: str) -> str:
    """Blank comments, string and char literals, keeping length and newlines."""
    out = list(src)
    n, i = len(src), 0

    def blank(a: int, b: int) -> None:
        for k in range(a, min(b, n)):
            if out[k] != "\n":
                out[k] = " "

    while i < n:
        c = src[i]
        if src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            blank(i, j)
            i = j
        elif src.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif src.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            blank(i, j)
            i = j
        elif c == "r" and re.match(r'r#*"', src[i : i + 40]) and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] == "_")):
            hashes = len(re.match(r"r(#*)", src[i:]).group(1))
            end = src.find('"' + "#" * hashes, i + 2 + hashes)
            j = n if end < 0 else end + 1 + hashes
            blank(i, j)
            i = j
        elif c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            blank(i, j + 1)
            i = j + 1
        elif c == "'":
            m = re.match(r"'(\\[^']*|[^\\'])'", src[i : i + 14])
            if m:
                blank(i, i + m.end())
                i += m.end()
            else:
                i += 1  # a lifetime
        else:
            i += 1
    return "".join(out)


TEST_ATTR = re.compile(r"#\s*!?\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]")
TEST_MOD = re.compile(r"\bmod\s+tests\b")


def test_ranges(src: str) -> list[tuple[int, int]]:
    """Return 1-based inclusive line ranges of test code in a Rust source.

    Test code is an item gated by `#[cfg(test)]`, a `mod tests`, or the whole
    file when it carries an inner `#![cfg(test)]`.
    """
    masked = mask_rust(src)
    starts = [0] + [m.end() for m in re.finditer("\n", masked)]

    def line_of(offset: int) -> int:
        lo, hi = 0, len(starts) - 1
        while lo < hi:
            mid = (lo + hi + 1) // 2
            if starts[mid] <= offset:
                lo = mid
            else:
                hi = mid - 1
        return lo + 1

    total = masked.count("\n") + 1
    ranges: list[tuple[int, int]] = []

    def item_end(pos: int) -> int:
        depth = 0
        while pos < len(masked):
            ch = masked[pos]
            if ch in "([":
                depth += 1
            elif ch in ")]":
                depth -= 1
            elif ch == "{":
                nest = 1
                pos += 1
                while pos < len(masked) and nest:
                    nest += {"{": 1, "}": -1}.get(masked[pos], 0)
                    pos += 1
                return pos
            elif ch == ";" and depth <= 0:
                return pos + 1
            pos += 1
        return len(masked)

    for match in TEST_ATTR.finditer(masked):
        if "!" in match.group(0):
            return [(1, total)]
        ranges.append((line_of(match.start()), line_of(item_end(match.end()) - 1)))
    for match in TEST_MOD.finditer(masked):
        ranges.append((line_of(match.start()), line_of(item_end(match.end()) - 1)))
    return ranges


def is_test_file(rel_in_package: str) -> bool:
    """A file that is test code by location or name, whatever its content."""
    parts = rel_in_package.split("/")
    name = parts[-1]
    stem = name.rsplit(".", 1)[0]
    return (
        parts[0] == "tests"
        or "tests" in parts[:-1]
        or (name.endswith(".rs") and (stem == "tests" or stem.endswith("_tests") or stem.endswith("_test")))
    )


def hunks(root: str, base: str, path: str) -> list[tuple[int, int, int, int]]:
    """Return (old_start, old_count, new_start, new_count) for every hunk."""
    out = git(root, "diff", "-U0", "--no-renames", base, "HEAD", "--", path).decode(errors="replace")
    found = []
    for match in re.finditer(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@", out, re.M):
        old_start, old_count, new_start, new_count = match.groups()
        found.append((int(old_start), int(old_count or 1), int(new_start), int(new_count or 1)))
    return found


def show(root: str, rev: str, path: str) -> str:
    try:
        return git(root, "show", f"{rev}:{path}").decode(errors="replace")
    except Exception:
        return ""


def touches_test_module(root: str, base: str, path: str) -> bool:
    """Whether the diff of a Rust file changes lines inside test code."""
    new_ranges = test_ranges(show(root, "HEAD", path))
    old_ranges = test_ranges(show(root, base, path))

    def overlaps(ranges: list[tuple[int, int]], start: int, count: int) -> bool:
        # A pure insertion or deletion sits between lines: count it against
        # the line it follows and the line it precedes.
        lo, hi = (start, start + count - 1) if count else (start, start + 1)
        return any(a <= hi and lo <= b for a, b in ranges)

    return any(
        overlaps(new_ranges, ns, nc) or overlaps(old_ranges, os_, oc)
        for os_, oc, ns, nc in hunks(root, base, path)
    )


def touched_packages(root: str, base: str, paths: list[str], rule: str) -> list[dict]:
    """Return the packages whose tests a merge gate must run.

    `narrow`: packages with changed test code (files under a tests/ directory
    or in test-named files, or Rust files whose diff touches a `#[cfg(test)]`
    item or `mod tests`). `wide`: every package any of whose files changed,
    apart from documentation. Each entry is
    {"package", "manifest", "args"}: `manifest` is null for the root workspace
    and the Cargo.toml path otherwise; `args` are extra test-runner arguments
    that narrow the run of the heavy daemon e2e package.
    """
    top = Path(root).resolve()
    found: dict[str, dict] = {}
    e2e_targets: dict[str, set[str]] = {}
    e2e_whole: set[str] = set()
    for path in paths:
        if path.startswith("vendor/"):
            continue
        owner = owning_package(top, path)
        if owner is None:
            continue
        name, package_dir, manifest = owner
        rel = path[len(package_dir) + 1 :] if package_dir != "." else path
        if rule == "wide":
            selected = not path.endswith((".md", ".txt"))
        else:
            selected = is_test_file(rel) or (path.endswith(".rs") and touches_test_module(root, base, path))
        if not selected:
            continue
        found.setdefault(name, {"package": name, "manifest": manifest, "args": []})
        if name in HEAVY_E2E_PACKAGES and rule == "narrow":
            # Only a top-level tests/<name>.rs maps to one test binary; any
            # other test file (shared helpers, fixtures) may feed all of them.
            parts = rel.split("/")
            if len(parts) == 2 and parts[0] == "tests" and parts[1].endswith(".rs"):
                e2e_targets.setdefault(name, set()).add(parts[1][:-3])
            elif rel.startswith("tests/"):
                e2e_whole.add(name)
    for name, entry in found.items():
        if name not in HEAVY_E2E_PACKAGES:
            continue
        if rule == "wide":
            entry["args"] = list(LIB_AND_BIN_ARGS)
        elif name not in e2e_whole:
            entry["args"] = [a for t in sorted(e2e_targets.get(name, ())) for a in ("--test", t)] + LIB_AND_BIN_ARGS
    return [found[name] for name in sorted(found)]


def classify(root: str, base_ref: str) -> tuple[str, list[str], str, bool]:
    """Return (mode, changed paths, reason, bundles_changed)."""
    if not base_ref or base_ref.startswith("-"):
        return "full", [], f"unusable base ref {base_ref!r}", True
    try:
        base = git(root, "merge-base", "HEAD", base_ref).decode().strip()
    except Exception as error:  # missing ref, no common ancestor, git failure
        return "full", [], f"no merge base with {base_ref} ({error})", True
    if not base:
        return "full", [], f"no merge base with {base_ref}", True
    out = git(root, "diff", "--name-only", "--no-renames", "-z", base, "HEAD")
    paths = sorted(name for name in out.decode().split("\0") if name)
    bundles_changed = any(p.startswith(BUNDLE_PREFIXES) for p in paths)
    if not paths:
        return "full", paths, "no changed paths", True
    outside = [p for p in paths if not p.startswith(UI_PREFIXES)]
    if outside:
        return "full", paths, f"paths outside the UI set, first: {outside[0]}", bundles_changed
    return "ui-only", paths, "every changed path is in the UI set", bundles_changed


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-ref", default="origin/next")
    parser.add_argument("--root", default=".")
    parser.add_argument("--rule", choices=("wide", "narrow"), default="wide")
    args = parser.parse_args()
    try:
        mode, paths, reason, bundles_changed = classify(args.root, args.base_ref)
    except BaseException as error:  # any fault means the full gate
        mode, paths, reason, bundles_changed = "full", [], f"classifier error: {error}", True
    try:
        if not paths and reason != "no changed paths":
            raise RuntimeError(reason)
        base = git(args.root, "merge-base", "HEAD", args.base_ref).decode().strip() if paths else ""
        packages = touched_packages(args.root, base, paths, args.rule)
        if mode == "ui-only":
            packages = [p for p in packages if p["package"] not in UI_GATE_PACKAGES]
        test_packages = json.dumps(packages, separators=(",", ":"))
    except BaseException as error:  # a fault must not read as "no tests to run"
        test_packages = "ERROR"
        reason += f"; test package detection failed: {error}"
    print(f"test_packages={test_packages}")
    print(f"mode={mode}")
    print(f"bundles_changed={'true' if bundles_changed else 'false'}")
    print(f"# classification: {mode} ({reason})")
    print(f"# changed paths ({len(paths)}):")
    for path in paths:
        print(f"#   {path}")
    sys.stdout.flush()


if __name__ == "__main__":
    main()
