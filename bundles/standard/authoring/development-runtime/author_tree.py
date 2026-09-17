#!/usr/bin/env python3
"""Author one exact publisher tree archive from a project-owned Config.

This is a publisher operation, not an admitted worker or an activation-target
transport.  It acquires only exact HTTPS inputs, constructs a fresh ordinary
tree, reproduces RyeOS's portable content identity, and emits a deterministic
archive.  Managed activation remains the sole node-side acquisition owner.
"""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import stat
import tarfile
import tempfile
import urllib.request

import yaml


SCHEMA = "ryeos.publisher_tree_acquisition.v1"
CONTENT_FILE_LIMIT = 32 * 1024 * 1024
LARGE_CHUNK_BYTES = 64 * 1024 * 1024
MAX_CONFIG_BYTES = 256 * 1024
MAX_SOURCES = 128


def sha256(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def canonical_json(value: object) -> bytes:
    return json.dumps(
        value, sort_keys=True, separators=(",", ":"), ensure_ascii=True
    ).encode("ascii")


def relative(value: object) -> PurePosixPath:
    if not isinstance(value, str):
        raise ValueError("tree member must be a string")
    path = PurePosixPath(value)
    if (
        path.is_absolute()
        or path.as_posix() != value
        or not path.parts
        or any(part in ("", ".", "..") for part in path.parts)
    ):
        raise ValueError(f"non-canonical tree member: {value!r}")
    return path


def lowercase_sha256(value: object) -> str:
    if (
        not isinstance(value, str)
        or len(value) != 64
        or any(character not in "0123456789abcdef" for character in value)
    ):
        raise ValueError("expected lowercase SHA-256")
    return value


def load_config(path: Path) -> dict:
    if path.is_symlink() or not path.is_file() or path.stat().st_size > MAX_CONFIG_BYTES:
        raise ValueError("acquisition Config must be one bounded regular file")
    value = yaml.safe_load(path.read_bytes())
    if not isinstance(value, dict) or value.get("schema") != SCHEMA:
        raise ValueError("unsupported publisher tree acquisition Config")
    required = {
        "category", "name", "version", "description", "schema",
        "source_date_epoch", "artifact", "sources",
    }
    if set(value) != required:
        raise ValueError("publisher tree acquisition Config has unknown fields")
    epoch = value["source_date_epoch"]
    if type(epoch) is not int or not 0 < epoch < 4_102_444_800:
        raise ValueError("invalid source date epoch")
    artifact = value["artifact"]
    if not isinstance(artifact, dict) or set(artifact) != {
        "release_tag", "archive", "prefix", "storage", "manifest_digest",
        "entries", "total_bytes",
    }:
        raise ValueError("invalid publisher artifact contract")
    for key in ("release_tag", "archive", "prefix"):
        relative(artifact[key])
        if len(PurePosixPath(artifact[key]).parts) != 1:
            raise ValueError(f"{key} must have one path component")
    if not artifact["archive"].endswith(".tar.gz"):
        raise ValueError("publisher artifact must be a gzip tar archive")
    if artifact["storage"] not in ("content", "large_content"):
        raise ValueError("unsupported manifest storage tier")
    lowercase_sha256(artifact["manifest_digest"])
    sources = value["sources"]
    if not isinstance(sources, list) or not 1 <= len(sources) <= MAX_SOURCES:
        raise ValueError("invalid publisher source count")
    targets: set[str] = set()
    directories: set[str] = set()
    total = 0
    for source in sources:
        if not isinstance(source, dict) or set(source) != {
            "target", "url", "bytes", "sha256", "mode"
        }:
            raise ValueError("invalid publisher source")
        target = relative(source["target"]).as_posix()
        if target in targets:
            raise ValueError("duplicate publisher target")
        targets.add(target)
        parent = PurePosixPath(target).parent
        while parent.as_posix() != ".":
            directories.add(parent.as_posix())
            parent = parent.parent
        if not isinstance(source["url"], str) or not source["url"].startswith("https://"):
            raise ValueError("publisher source requires HTTPS")
        size = source["bytes"]
        if type(size) is not int or not 0 < size <= 64 * 1024**3:
            raise ValueError("invalid publisher source byte bound")
        total += size
        lowercase_sha256(source["sha256"])
        if source["mode"] not in (0o644, 0o755):
            raise ValueError("publisher source mode is not portable")
    if artifact["entries"] != len(sources) + len(directories) or artifact["total_bytes"] != total:
        raise ValueError("artifact summary disagrees with its exact sources")
    return value


def obtain(cache: Path, source: dict, offline: bool) -> Path:
    target = relative(source["target"])
    cache_name = source["sha256"] + "-" + target.name
    destination = cache / cache_name
    if destination.is_file() and not destination.is_symlink():
        if destination.stat().st_size != source["bytes"] or sha256(destination) != source["sha256"]:
            raise ValueError(f"cached publisher input has the wrong identity: {target}")
        return destination
    if destination.exists() or destination.is_symlink():
        raise ValueError(f"publisher cache path is not an ordinary file: {destination}")
    if offline:
        raise ValueError(f"offline authoring is missing {target}")
    request = urllib.request.Request(
        source["url"], headers={"User-Agent": "RyeOS-publisher-tree-authoring/1"}
    )
    with tempfile.NamedTemporaryFile(dir=cache, delete=False) as output:
        temporary = Path(output.name)
        try:
            with urllib.request.urlopen(request, timeout=60) as response:
                if not response.url.startswith("https://"):
                    raise ValueError("publisher source redirected away from HTTPS")
                observed = 0
                while chunk := response.read(1024 * 1024):
                    observed += len(chunk)
                    if observed > source["bytes"]:
                        raise ValueError(f"publisher source exceeds its byte bound: {target}")
                    output.write(chunk)
            output.flush()
            os.fsync(output.fileno())
            if observed != source["bytes"] or sha256(temporary) != source["sha256"]:
                raise ValueError(f"downloaded publisher source has the wrong identity: {target}")
            os.link(temporary, destination)
        finally:
            temporary.unlink(missing_ok=True)
    return destination


def copy_inputs(config: dict, cache: Path, tree: Path, offline: bool) -> None:
    tree.mkdir(mode=0o700)
    for source in config["sources"]:
        selected = obtain(cache, source, offline)
        target = tree.joinpath(*relative(source["target"]).parts)
        target.parent.mkdir(parents=True, exist_ok=True, mode=0o755)
        with selected.open("rb") as input_stream, target.open("xb") as output_stream:
            shutil.copyfileobj(input_stream, output_stream, length=1024 * 1024)
        target.chmod(source["mode"])
    for directory, subdirs, files in os.walk(tree):
        subdirs.sort()
        files.sort()
        Path(directory).chmod(0o755)


def manifest(tree: Path, storage: str) -> tuple[dict, str]:
    entries: list[dict] = []
    total = 0
    for path in sorted(tree.rglob("*"), key=lambda item: item.relative_to(tree).as_posix().encode()):
        name = path.relative_to(tree).as_posix()
        info = path.lstat()
        if stat.S_ISDIR(info.st_mode):
            entries.append({"path": name, "kind": "dir"})
            continue
        if not stat.S_ISREG(info.st_mode):
            raise ValueError("publisher tree contains a link or special file")
        mode = 0o755 if info.st_mode & 0o111 else 0o644
        total += info.st_size
        entry = {"path": name, "kind": "file", "mode": mode, "size": info.st_size}
        if storage == "content" or info.st_size <= CONTENT_FILE_LIMIT:
            entry["blob_hash"] = sha256(path)
        else:
            chunks = []
            whole = hashlib.sha256()
            with path.open("rb") as stream:
                while chunk := stream.read(LARGE_CHUNK_BYTES):
                    whole.update(chunk)
                    chunks.append(hashlib.sha256(chunk).hexdigest())
            entry.update(
                file_sha256=whole.hexdigest(),
                chunk_size=LARGE_CHUNK_BYTES,
                chunk_hashes=chunks,
            )
        entries.append(entry)
    if storage == "content":
        value = {
            "schema": "ryeos.external_content.tree.v2",
            "kind": "external_content_manifest",
        }
    else:
        value = {
            "schema": "ryeos.external_content.large.v2",
            "kind": "external_large_content_manifest",
        }
    value.update(entries=entries, entry_count=len(entries), total_bytes=total)
    return value, hashlib.sha256(canonical_json(value)).hexdigest()


def archive_tree(tree: Path, output: Path, prefix: str, epoch: int) -> None:
    if output.exists() or output.is_symlink():
        raise ValueError("publisher output archive already exists")
    temporary = output.with_name("." + output.name + ".tmp")
    if temporary.exists() or temporary.is_symlink():
        raise ValueError("publisher output staging path already exists")
    with temporary.open("xb") as raw:
        with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch) as compressed:
            with tarfile.open(fileobj=compressed, mode="w|", format=tarfile.PAX_FORMAT) as archive:
                root = tarfile.TarInfo(prefix + "/")
                root.type = tarfile.DIRTYPE
                root.mode = 0o755
                root.uid = root.gid = 0
                root.uname = root.gname = ""
                root.mtime = epoch
                archive.addfile(root)
                for path in sorted(tree.rglob("*"), key=lambda item: item.relative_to(tree).as_posix().encode()):
                    relative_name = path.relative_to(tree).as_posix()
                    info = path.lstat()
                    member = tarfile.TarInfo(prefix + "/" + relative_name + ("/" if path.is_dir() else ""))
                    member.type = tarfile.DIRTYPE if path.is_dir() else tarfile.REGTYPE
                    member.mode = 0o755 if path.is_dir() or info.st_mode & 0o111 else 0o644
                    member.uid = member.gid = 0
                    member.uname = member.gname = ""
                    member.mtime = epoch
                    if path.is_file():
                        member.size = info.st_size
                        with path.open("rb") as stream:
                            archive.addfile(member, stream)
                    else:
                        archive.addfile(member)
        raw.flush()
        os.fsync(raw.fileno())
    os.replace(temporary, output)


def author(config_path: Path, cache: Path, output_directory: Path, offline: bool) -> dict:
    config = load_config(config_path)
    artifact = config["artifact"]
    cache.mkdir(parents=True, exist_ok=True)
    if cache.is_symlink() or not cache.is_dir():
        raise ValueError("publisher cache is not an ordinary directory")
    output_directory.mkdir(parents=True, exist_ok=True)
    if output_directory.is_symlink() or not output_directory.is_dir():
        raise ValueError("publisher output is not an ordinary directory")
    with tempfile.TemporaryDirectory(dir=output_directory, prefix=".tree-") as temporary:
        tree = Path(temporary) / "tree"
        copy_inputs(config, cache, tree, offline)
        observed, digest = manifest(tree, artifact["storage"])
        if (
            digest != artifact["manifest_digest"]
            or observed["entry_count"] != artifact["entries"]
            or observed["total_bytes"] != artifact["total_bytes"]
        ):
            raise ValueError("authored tree differs from its signed RyeOS identity")
        output = output_directory / artifact["archive"]
        archive_tree(tree, output, artifact["prefix"], config["source_date_epoch"])
    return {
        "schema": "ryeos.publisher_tree_artifact.v1",
        "release_tag": artifact["release_tag"],
        "archive": artifact["archive"],
        "archive_bytes": output.stat().st_size,
        "archive_sha256": sha256(output),
        "prefix": artifact["prefix"],
        "storage": artifact["storage"],
        "manifest_digest": digest,
        "entries": observed["entry_count"],
        "total_bytes": observed["total_bytes"],
    }


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--offline", action="store_true")
    arguments = parser.parse_args()
    print(json.dumps(author(arguments.config, arguments.cache, arguments.output, arguments.offline), sort_keys=True))


if __name__ == "__main__":
    main()
