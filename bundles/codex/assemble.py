#!/usr/bin/env python3
"""Assemble the exact OpenAI Codex executable admitted by the Codex bundle."""

from __future__ import annotations

import argparse
import hashlib
from pathlib import Path
import shutil
import stat
import tarfile
import tempfile
import urllib.request

VERSION = "0.147.0"
TARGET = "x86_64-unknown-linux-musl"
ARCHIVE_NAME = f"codex-{TARGET}.tar.gz"
ARCHIVE_SHA256 = "0246e2e773834e07f0fb5249ed6ebad12e4591e608f8c7bb97dd6a9690544c36"
EXECUTABLE_SHA256 = "cb0a15567e9a60a5820d54b0f6ae86d504dc3805c1eab21a47f70e3eb7b73a40"
URL = (
    f"https://github.com/openai/codex/releases/download/rust-v{VERSION}/"
    f"{ARCHIVE_NAME}"
)


def digest(path: Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as source:
        for chunk in iter(lambda: source.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def obtain(cache: Path, offline: bool) -> Path:
    archive = cache / ARCHIVE_NAME
    if archive.is_file() and digest(archive) == ARCHIVE_SHA256:
        return archive
    if archive.exists():
        raise RuntimeError(f"cached archive has the wrong digest: {archive}")
    if offline:
        raise RuntimeError(f"offline assembly is missing {archive}")
    temporary = cache / f".{ARCHIVE_NAME}.download"
    request = urllib.request.Request(URL, headers={"User-Agent": "RyeOS-Codex-assembly/1"})
    with urllib.request.urlopen(request) as response, temporary.open("xb") as output:
        shutil.copyfileobj(response, output, 1024 * 1024)
    if digest(temporary) != ARCHIVE_SHA256:
        temporary.unlink(missing_ok=True)
        raise RuntimeError("downloaded Codex archive has the wrong digest")
    temporary.replace(archive)
    return archive


def assemble(cache: Path, output: Path, offline: bool) -> None:
    cache.mkdir(parents=True, exist_ok=True)
    if output.exists():
        raise RuntimeError(f"output already exists: {output}")
    archive = obtain(cache, offline)
    with tempfile.TemporaryDirectory(prefix="ryeos-codex-") as scratch_name:
        scratch = Path(scratch_name)
        with tarfile.open(archive, "r:gz") as source:
            members = source.getmembers()
            expected = f"codex-{TARGET}"
            if len(members) != 1 or members[0].name != expected or not members[0].isfile():
                raise RuntimeError("Codex archive does not contain its one expected regular file")
            extracted = source.extractfile(members[0])
            if extracted is None:
                raise RuntimeError("Codex archive executable could not be opened")
            candidate = scratch / "codex"
            with candidate.open("xb") as destination:
                shutil.copyfileobj(extracted, destination, 1024 * 1024)
        if digest(candidate) != EXECUTABLE_SHA256:
            raise RuntimeError("Codex executable does not match the pinned digest")
        candidate.chmod(stat.S_IRUSR | stat.S_IWUSR | stat.S_IXUSR |
                        stat.S_IRGRP | stat.S_IXGRP | stat.S_IROTH | stat.S_IXOTH)
        output.mkdir(parents=True)
        candidate.replace(output / "codex")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    assemble(args.cache.resolve(), args.output.resolve(), args.offline)


if __name__ == "__main__":
    main()
