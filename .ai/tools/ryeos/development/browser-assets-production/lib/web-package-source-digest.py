#!/usr/bin/env python3
# ryeos:signed:2026-11-20T10:04:57Z:b070df195eaa1a84eb4aa523cfc68507edecd323f005ae4f7b91fe1fbb9c70d3:ZZaOsVf9sq8s9TfH0Kpwt8xpxrRC/F2OtWN5JRPE6XrGJ1naCFUtM/dsTOQ5DPaecGDuhvvC5/xTtySVcuj4Bg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# Externally invoked host helper in the development bundle; lib/ is excluded
# from Tool resolution, so this is signed source, not an admitted Tool. It
# grants no RyeOS authority. Run it from a checkout of this repository.
# Digest of the sources the checked-in browser package is built from; CI compares it with the one recorded in web-assets.json.
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

The renderer lock is hashed with the `version` of its source-less entries (the
workspace path crates) masked. A release bumps those versions in every lock, but
a path crate's version reaches the wasm build only through its own Cargo.toml
(wasm-bindgen's per-crate symbol hash reads CARGO_PKG_VERSION for the crates
that use it), and the manifests of the crates in the wasm32 closure are digest
inputs themselves, hashed unmasked. A path crate outside the closure has no
effect on the package at all. Registry and git entries, checksums and
dependency edges stay hashed.

  --print   write the digest to stdout
  --check   compare it with the digest recorded in web-assets.json
  --write-stamp DIR DIGEST
            record that the files in DIR were built from sources with DIGEST
  --stamped print the digest the package was built from; refuse unless the
            build stamp exists, its package files are the ones now in pkg/,
            and its digest is the current sources' digest

The build stamp (an untracked file beside pkg/, written only by a successful
build, removed when a build starts) is what lets the registry generator record a
digest: `--refresh-digests` copies the digest from the stamp and nothing else,
so a package that was not rebuilt from the current sources cannot be certified
fresh.
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

RENDERER_LOCK = "crates/clients/rune/Cargo.lock"
MASKED_VERSION = b'version = "<path crate>"'
PACKAGE = "crates/clients/rune/pkg"
STAMP = "crates/clients/rune/.pkg-build-stamp"
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


def mask_path_crate_versions(lock: bytes) -> bytes:
    """Blank the `version` line of every lock package that has no `source`."""
    blocks = lock.split(b"[[package]]")
    for i, block in enumerate(blocks[1:], start=1):
        lines = block.split(b"\n")
        if any(line.startswith(b"source = ") for line in lines):
            continue
        blocks[i] = b"\n".join(
            MASKED_VERSION if line.startswith(b"version = ") else line for line in lines
        )
    return b"[[package]]".join(blocks)


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
        content = (root / name).read_bytes()
        if name == RENDERER_LOCK:
            content = mask_path_crate_versions(content)
        inner = hashlib.sha256(content).hexdigest()
        outer.update(f"{name}\0{inner}\n".encode())
    return outer.hexdigest()


def package_hash(directory: Path) -> str:
    outer = hashlib.sha256()
    for path in sorted(p for p in directory.iterdir() if p.is_file()):
        outer.update(f"{path.name}\0{hashlib.sha256(path.read_bytes()).hexdigest()}\n".encode())
    return outer.hexdigest()


def write_stamp(root: Path, directory: Path, built_from: str) -> None:
    (root / STAMP).write_text(f"{built_from}\n{package_hash(directory)}\n")


def stamped_digest(root: Path) -> str:
    stamp = root / STAMP
    if not stamp.is_file():
        raise SystemExit(f"no build stamp ({STAMP}): the package was not built here; run the build")
    built_from, _, built_package = stamp.read_text().strip().partition("\n")
    if package_hash(root / PACKAGE) != built_package:
        raise SystemExit(f"{PACKAGE} is not the package the last build produced; run the build")
    if built_from != digest(root):
        raise SystemExit("the package was built from different sources than the current ones; run the build")
    return built_from


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--root", default=str(Path(__file__).resolve().parents[6]))
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--print", action="store_true")
    mode.add_argument("--check", action="store_true")
    mode.add_argument("--stamped", action="store_true")
    mode.add_argument("--write-stamp", nargs=2, metavar=("DIR", "DIGEST"))
    args = parser.parse_args()
    root = Path(args.root)
    if args.write_stamp:
        write_stamp(root, Path(args.write_stamp[0]), args.write_stamp[1])
        return
    if args.stamped:
        print(stamped_digest(root))
        return
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
