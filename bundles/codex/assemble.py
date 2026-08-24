#!/usr/bin/env python3
"""Assemble the exact OpenAI Codex workload files admitted by the bundle."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import stat
import tarfile
import tempfile
import urllib.request

VERSION = "0.147.0"
TARGET = "x86_64-unknown-linux-musl"
ARCHIVE_NAME = f"codex-package-{TARGET}.tar.gz"
ARCHIVE_SHA256 = "bd758d53d56e41dc65e045f4589df79a038ed197a011adcb52a258e6ad64cfda"
EXECUTABLE_SHA256 = "cb0a15567e9a60a5820d54b0f6ae86d504dc3805c1eab21a47f70e3eb7b73a40"
CODE_MODE_HOST_SHA256 = "00ecf5d040865b97884c488883abd342581c2a432debe7a54e4646bceee3d2d6"
BWRAP_SHA256 = "77360cb751ccedc5971391444ac86a8a33c15b04d6b4a6fe45f5d25496e62c4c"
ZSH_SHA256 = "67faaaa89242c4a332e16e508a1977cffc24bf7fca31d4411cdfd101f3831ef3"
RG_SHA256 = "e62198eb19b136b88c330af83647b5a962cb99b6b1f066758568f12de1974849"
URL = f"https://releases.openai.com/codex/releases/{VERSION}/{ARCHIVE_NAME}"


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
            by_name = {member.name: member for member in members}
            if len(by_name) != len(members):
                raise RuntimeError("Codex package archive contains duplicate paths")
            required = {
                "bin/codex": "codex",
                "bin/codex-code-mode-host": "codex-code-mode-host",
                "codex-resources/bwrap": "codex-resources/bwrap",
                "codex-resources/zsh/bin/zsh": "codex-resources/zsh/bin/zsh",
                "codex-path/rg": "codex-path/rg",
                "codex-package.json": "codex-package.json",
            }
            candidates = {}
            for archive_name, output_name in required.items():
                member = by_name.get(archive_name)
                if member is None or not member.isfile():
                    raise RuntimeError(
                        f"Codex package is missing regular file {archive_name}"
                    )
                extracted = source.extractfile(member)
                if extracted is None:
                    raise RuntimeError(f"Codex package file {archive_name} could not be opened")
                candidate = scratch / output_name
                candidate.parent.mkdir(parents=True, exist_ok=True)
                with candidate.open("xb") as destination:
                    shutil.copyfileobj(extracted, destination, 1024 * 1024)
                candidates[output_name] = candidate
        candidate = candidates["codex"]
        code_mode_host = candidates["codex-code-mode-host"]
        bwrap = candidates["codex-resources/bwrap"]
        zsh = candidates["codex-resources/zsh/bin/zsh"]
        rg = candidates["codex-path/rg"]
        package = json.loads(candidates["codex-package.json"].read_text(encoding="utf-8"))
        if package != {
            "layoutVersion": 1,
            "version": VERSION,
            "target": TARGET,
            "variant": "codex",
            "entrypoint": "bin/codex",
            "resourcesDir": "codex-resources",
            "pathDir": "codex-path",
        }:
            raise RuntimeError("Codex package metadata does not match the pinned layout")
        if digest(candidate) != EXECUTABLE_SHA256:
            raise RuntimeError("Codex executable does not match the pinned digest")
        if digest(code_mode_host) != CODE_MODE_HOST_SHA256:
            raise RuntimeError("Codex code-mode host does not match the pinned digest")
        if digest(bwrap) != BWRAP_SHA256:
            raise RuntimeError("Codex Bubblewrap resource does not match the pinned digest")
        if digest(zsh) != ZSH_SHA256:
            raise RuntimeError("Codex shell resource does not match the pinned digest")
        if digest(rg) != RG_SHA256:
            raise RuntimeError("Codex rg resource does not match the pinned digest")
        executable_mode = (
            stat.S_IRUSR | stat.S_IWUSR | stat.S_IXUSR |
            stat.S_IRGRP | stat.S_IXGRP | stat.S_IROTH | stat.S_IXOTH
        )
        for executable in (candidate, code_mode_host, bwrap, zsh, rg):
            executable.chmod(executable_mode)
        output.mkdir(parents=True)
        for output_name in (
            "codex",
            "codex-code-mode-host",
            "codex-resources/bwrap",
            "codex-resources/zsh/bin/zsh",
            "codex-path/rg",
        ):
            destination = output / output_name
            destination.parent.mkdir(parents=True, exist_ok=True)
            candidates[output_name].replace(destination)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--offline", action="store_true")
    args = parser.parse_args()
    assemble(args.cache.resolve(), args.output.resolve(), args.offline)


if __name__ == "__main__":
    main()
