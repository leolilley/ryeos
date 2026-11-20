#!/usr/bin/env python3
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

  --base-ref REF   the ref the branch will merge into (default origin/next)
  --root DIR       repository to classify (default: this checkout)
"""

from __future__ import annotations

import argparse
import subprocess
import sys

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
    args = parser.parse_args()
    try:
        mode, paths, reason, bundles_changed = classify(args.root, args.base_ref)
    except BaseException as error:  # any fault means the full gate
        mode, paths, reason, bundles_changed = "full", [], f"classifier error: {error}", True
    print(f"mode={mode}")
    print(f"bundles_changed={'true' if bundles_changed else 'false'}")
    print(f"# classification: {mode} ({reason})")
    print(f"# changed paths ({len(paths)}):")
    for path in paths:
        print(f"#   {path}")
    sys.stdout.flush()


if __name__ == "__main__":
    main()
