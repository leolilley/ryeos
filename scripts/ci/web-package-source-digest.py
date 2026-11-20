#!/usr/bin/env python3
"""Digest of the sources that determine the checked-in browser package.

The package under crates/clients/rune/pkg is built by a slow wasm build and is
committed. Nothing else in CI notices when a source feeding that build changes
but the package is not rebuilt: the package, its manifest and the generated
registry stay mutually consistent while being out of date. This script
reduces the build inputs to one digest. A package rebuild records it as
`source_digest` in crates/daemon/ryeos-ui/web-assets.json (the registry
generator calls `--print`), and CI compares the two with `--check`.

Inputs are git-tracked files only (`git ls-files`), hashed by content; no
mtimes, no untracked files, no build output. Each entry below is a file or a
directory prefix; every entry must match at least one tracked file, so a
rename cannot silently drop an input from the digest. Directory entries skip
`tests/` directories, which the wasm build never compiles. The pinned renderer
source revision is named by the renderer manifest and lock, and the toolchain
by rust-toolchain.toml; both are inputs.

  --print   write the digest to stdout
  --check   compare it with the digest recorded in web-assets.json
"""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys
from pathlib import Path

INPUTS = (
    # The renderer crate (page entry, manifest, lock naming the pinned renderer
    # source revision, page shell, boot script, coverage fonts).
    "crates/clients/rune/src/",
    "crates/clients/rune/Cargo.toml",
    "crates/clients/rune/Cargo.lock",
    "crates/clients/rune/web/index.html",
    "crates/clients/rune/web/ryeos_boot.js",
    "crates/clients/rune/web/fonts/",
    # The other local crates in the wasm32 dependency closure.
    "crates/clients/base/",
    "crates/clients/runtime/",
    "crates/clients/presentation-contract/",
    "crates/engine/ryeos-timeline/",
    # The build recipe and the toolchain.
    ".ai/tools/ryeos/development/browser-assets-production/lib/rune-snippets.toml",
    ".ai/tools/ryeos/development/browser-assets-production/lib/build-rune-web.sh",
    "rust-toolchain.toml",
)

MANIFEST = "crates/daemon/ryeos-ui/web-assets.json"
REBUILD = ".ai/tools/ryeos/development/browser-assets-production/publish.sh --publish"


def tracked(root: Path) -> list[str]:
    out = subprocess.run(
        ["git", "-C", str(root), "ls-files", "-z"], check=True, capture_output=True
    ).stdout
    return [name for name in out.decode().split("\0") if name]


def selected(entry: str, names: list[str]) -> list[str]:
    if entry.endswith("/"):
        found = [n for n in names if n.startswith(entry) and "/tests/" not in n[len(entry) - 1 :]]
    else:
        found = [n for n in names if n == entry]
    return found


def digest(root: Path) -> str:
    names = tracked(root)
    paths: set[str] = set()
    for entry in INPUTS:
        found = selected(entry, names)
        if not found:
            raise SystemExit(f"package input matches no tracked file: {entry}")
        paths.update(found)
    outer = hashlib.sha256()
    for name in sorted(paths):
        inner = hashlib.sha256((root / name).read_bytes()).hexdigest()
        outer.update(f"{name}\0{inner}\n".encode())
    return outer.hexdigest()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", default=str(Path(__file__).resolve().parents[2]))
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--print", action="store_true")
    mode.add_argument("--check", action="store_true")
    args = parser.parse_args()
    root = Path(args.root)
    current = digest(root)
    if args.print:
        print(current)
        return
    recorded = json.loads((root / MANIFEST).read_text()).get("source_digest")
    if recorded == current:
        print(f"web package is fresh: source digest {current}")
        return
    print(
        "The browser package is stale: the sources it is built from changed "
        "since it was last rebuilt (or the package never recorded a source digest).\n"
        f"  recorded in {MANIFEST}: {recorded}\n"
        f"  computed from the tracked sources: {current}\n"
        f"Rebuild and commit the package: {REBUILD}",
        file=sys.stderr,
    )
    sys.exit(1)


if __name__ == "__main__":
    main()
