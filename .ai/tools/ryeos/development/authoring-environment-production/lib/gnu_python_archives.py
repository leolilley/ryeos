# ryeos:signed:2026-09-08T10:38:22Z:f7c145f3256f9d83e46e35177a26df7eb4e6e6ad1967f588fa4227ec0dd95c06:/p9+leICQBM853xWJcaU6k+knkDebpU0LsSAFArwRe9HhNHz856wzLuuxRmY4iJpSt+jCrt3IyUCgTs9DgA9Dw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Finite GNU Python archive selection into a caller-owned private directory.

No acquisition, interpreter execution, manifest publication, or host discovery.
The caller supplies exact admitted archive identities. Existing
archives.open_archive owns bounded stdlib compression and
tar decoding. Files are written before links, so archive links never steer writes.
"""

from __future__ import annotations

from contextlib import contextmanager
from dataclasses import dataclass
import hashlib
import json
import os
from pathlib import Path
import re
import stat

from archives import MAX_EXPANDED_BYTES, open_archive, resolve_contained_member
from production import relative


@dataclass(frozen=True)
class Limits:
    entries: int = 16384
    file_bytes: int = 128 * 1024 * 1024
    total_bytes: int = 256 * 1024 * 1024
    expanded_bytes: int = MAX_EXPANDED_BYTES

    def validate(self):
        for value, ceiling in ((self.entries, 100000), (self.file_bytes, 128 * 1024**2),
                               (self.total_bytes, 1024**3), (self.expanded_bytes, MAX_EXPANDED_BYTES)):
            if type(value) is not int or not 0 < value <= ceiling:
                raise ValueError("invalid GNU archive bound")


@contextmanager
def checked_archive(path, expected_sha256, expected_bytes):
    if not isinstance(expected_sha256, str) or not re.fullmatch(r"[0-9a-f]{64}", expected_sha256):
        raise ValueError("archive requires exact lowercase SHA-256")
    if type(expected_bytes) is not int or not 0 < expected_bytes <= 1024**3:
        raise ValueError("invalid archive byte bound")
    descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(descriptor, "rb", buffering=0) as stream:
        before = os.fstat(stream.fileno())
        if not stat.S_ISREG(before.st_mode) or before.st_size != expected_bytes:
            raise ValueError("archive is not an exact regular input")
        if hashlib.file_digest(stream, "sha256").hexdigest() != expected_sha256:
            raise ValueError("archive SHA-256 mismatch")
        stream.seek(0)
        yield stream
        stream.seek(0)
        after = os.fstat(stream.fileno())
        if (before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (
                after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns):
            raise ValueError("archive changed during selection")
        if hashlib.file_digest(stream, "sha256").hexdigest() != expected_sha256:
            raise ValueError("archive bytes changed during selection")


def selected(name, metadata):
    return (name == "python/PYTHON.json" or name == "python/licenses"
            or name.startswith("python/licenses/")) if metadata else (name == "python" or name.startswith("python/"))


def _extract(archive, destination, *, expected_sha256, expected_bytes, metadata,
             limits):
    limits.validate()
    destination = Path(destination)
    if destination.exists() or destination.is_symlink():
        raise ValueError("archive destination must be fresh")
    entries, directories, output_names, total = {}, set(), set(), 0
    with checked_archive(archive, expected_sha256, expected_bytes) as raw:
        with open_archive(raw, maximum_expanded_bytes=limits.expanded_bytes) as opened:
            destination.mkdir(mode=0o700)
            for member in opened:
                name = member.name.removesuffix("/") if member.isdir() else member.name
                relative(name)
                if not selected(name, metadata):
                    continue
                if name in entries:
                    raise ValueError("duplicate or excessive selected archive entries")
                if member.isdir():
                    kind, value = "directory", None
                elif member.isfile():
                    kind, value = "file", member.size
                    total += member.size
                    if member.size > limits.file_bytes or total > limits.total_bytes:
                        raise ValueError("selected archive bytes exceed their bounds")
                elif member.issym() or member.islnk():
                    kind = "hardlink" if member.islnk() else "symlink"
                    value = None
                else:
                    raise ValueError("special archive entry is not admitted")
                # No links are materialized until every archive write has
                # finished. Still reject a logical link/file ancestor.
                for parent in Path(name).parents:
                    if str(parent) == ".":
                        break
                    if parent.as_posix() in entries and entries[parent.as_posix()][0] != "directory":
                        raise ValueError("archive entry has a non-directory ancestor")
                    directories.add(parent.as_posix())
                    output_names.add(parent.as_posix())
                if kind != "directory" and name in directories:
                    raise ValueError("archive entry replaces an existing directory")
                if kind == "directory":
                    directories.add(name)
                entries[name] = (kind, value, member.linkname)
                output_names.add(name)
                if len(output_names) > limits.entries:
                    raise ValueError("excessive selected archive entries and directories")
                target = destination / name
                target.parent.mkdir(parents=True, exist_ok=True)
                if kind == "directory":
                    target.mkdir(exist_ok=True)
                elif kind == "file":
                    with opened.extractfile(member) as source, target.open("xb") as output:
                        remaining = member.size
                        while remaining:
                            data = source.read(min(65536, remaining))
                            if not data:
                                raise ValueError("truncated selected archive file")
                            output.write(data)
                            remaining -= len(data)
                    target.chmod(0o755 if member.mode & 0o111 else 0o644)
    if not entries or not any(value[0] == "file" for value in entries.values()):
        raise ValueError("Python archive selection is empty")
    # The existing archive owner also serves post-extraction ELF closure
    # checks. Preserve raw targets; do not normalize away link/parent ordering.
    nodes = {name: ("directory", None) for name in directories}
    nodes.update({name: (kind, raw if kind in ("symlink", "hardlink") else None)
                  for name, (kind, _, raw) in entries.items()})
    hardlinks = []
    for name, (kind, target, original) in entries.items():
        if kind in ("hardlink", "symlink"):
            resolved, target_kind = resolve_contained_member(nodes, name, selected_root="python",
                                                            max_hops=min(limits.entries, 128))
            if not selected(resolved, metadata):
                raise ValueError("resolved archive link leaves selected Python content")
            if kind == "hardlink":
                if target_kind != "file":
                    raise ValueError("archive hardlink does not name a regular file")
                hardlinks.append((name, resolved))
    for name, resolved in hardlinks:
        total += (destination / resolved).stat().st_size
        if total > limits.total_bytes:
            raise ValueError("expanded hardlink content exceeds product byte bound")
        os.link(destination / resolved, destination / name)
    for name, (kind, target, original) in entries.items():
        if kind == "symlink":
            os.symlink(original, destination / name)
    for directory, _, _ in os.walk(destination, followlinks=False):
        Path(directory).chmod(0o755)
    return {"archive_sha256": expected_sha256, "archive_bytes": expected_bytes,
            "selected_entries": len(output_names), "regular_bytes": total}


def extract_install(archive, destination, *, expected_sha256, expected_bytes,
                    limits=Limits()):
    return _extract(archive, destination, expected_sha256=expected_sha256,
                    expected_bytes=expected_bytes, metadata=False, limits=limits)


def extract_metadata(archive, destination, *, expected_sha256, expected_bytes,
                     limits=Limits(total_bytes=16 * 1024**2)):
    """Preserve upstream metadata/licenses without claiming license closure.

    The producer must separately validate every referenced license against its
    pinned GNU source evidence; musl-specific exceptions are not transferable.
    """
    receipt = _extract(archive, destination, expected_sha256=expected_sha256,
                       expected_bytes=expected_bytes, metadata=True,
                       limits=limits)
    metadata = Path(destination) / "python/PYTHON.json"
    if metadata.is_symlink() or not metadata.is_file() or metadata.stat().st_size > 1024**2:
        raise ValueError("Python archive lacks bounded regular PYTHON.json")
    value = json.loads(metadata.read_bytes())
    if not isinstance(value, dict):
        raise ValueError("PYTHON.json must be an object")
    if not (Path(destination) / "python/licenses").is_dir():
        raise ValueError("Python archive lacks selected licenses")
    return receipt, value
