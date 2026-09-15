# ryeos:signed:2026-09-08T10:38:22Z:6414b540a5c2bbe4075356fe425841a3fc96642051af2b5dc19bbd8c46e3cde3:XCBxJJ10sAql6I21jzVHNeXVH36R2zTj9rSKDqBkFwlh9ktR/7DnAa+2Tu2bZylrd9VY3aXk4caZICWERzpWCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Bounded selection from admitted source archives; no filesystem extraction."""

from __future__ import annotations

import bz2
from collections import deque
from contextlib import ExitStack, contextmanager
import gzip
import lzma
from pathlib import Path
import tarfile

from production import MAX_FILE_BYTES, MAX_TOTAL_BYTES, relative

MAX_ARCHIVE_ENTRIES = 100_000
MAX_EXPANDED_BYTES = 3 * 1024 * 1024 * 1024
MAX_METADATA_BYTES = 64 * 1024
MAX_TOTAL_METADATA_BYTES = 8 * 1024 * 1024


def resolve_contained_member(nodes, name: str, *, selected_root: str = "",
                             max_hops: int = 128) -> tuple[str, str]:
    """Resolve a bounded, complete member graph without touching a filesystem.

    nodes maps canonical member names to (file/directory/symlink/hardlink,
    raw target or None). Symlinks start at their parent; archive hardlinks
    start at archive root. Crucially a/../b follows a before interpreting '..'.
    The selected root is an exact member prefix, never a host pathname.
    """
    if not isinstance(nodes, dict) or len(nodes) > MAX_ARCHIVE_ENTRIES:
        raise ValueError("member graph exceeds its entry bound")
    if type(max_hops) is not int or not 0 < max_hops <= 128:
        raise ValueError("invalid member link-hop bound")
    relative(name)
    boundary = relative(selected_root).parts if selected_root else ()
    if boundary and tuple(name.split("/")[:len(boundary)]) != boundary:
        raise ValueError("member is outside the selected root")
    pending, components, hops = deque(name.split("/")), [], 0
    while pending:
        component = pending.popleft()
        if component == ".":
            continue
        if component == "..":
            if len(components) <= len(boundary):
                raise ValueError("member link escapes selected root through another link")
            components.pop()
            continue
        components.append(component)
        prefix = "/".join(components)
        node = nodes.get(prefix)
        if not isinstance(node, tuple) or len(node) != 2:
            raise ValueError("member link target is missing or malformed")
        kind, raw_target = node
        if kind in ("symlink", "hardlink"):
            hops += 1
            if hops > max_hops:
                raise ValueError("cyclic or excessively deep member link")
            if (not isinstance(raw_target, str) or not raw_target
                    or len(raw_target) > 1024 or raw_target.startswith("/")
                    or "\\" in raw_target or "\0" in raw_target):
                raise ValueError("unsafe member link target")
            target_parts = raw_target.split("/")
            for part in target_parts:
                if part not in (".", ".."):
                    relative(part)
            components = [] if kind == "hardlink" else components[:-1]
            pending.extendleft(reversed(target_parts))
        elif kind not in ("file", "directory") or raw_target is not None:
            raise ValueError("invalid member graph entry")
        elif pending and kind != "directory":
            raise ValueError("member link traverses a non-directory")
    target = "/".join(components)
    if boundary and tuple(components[:len(boundary)]) != boundary:
        raise ValueError("resolved member leaves selected root")
    if not target and not boundary:
        return "", "directory"
    node = nodes.get(target)
    if node is None or node[0] not in ("file", "directory"):
        raise ValueError("member link does not resolve to retained content")
    return target, node[0]


@contextmanager
def open_archive(path_or_fileobj, *, maximum_expanded_bytes: int = MAX_EXPANDED_BYTES):
    """Bound stdlib decoding and hidden metadata before internal allocation.

    PAX/GNU headers are consumed before tarfile yields a logical member. Count
    their standard parsed headers too, and limit their size before tarfile reads
    the body. No archive fields are parsed here.
    """
    with ExitStack() as stack:
        raw = (stack.enter_context(open(path_or_fileobj, "rb"))
               if isinstance(path_or_fileobj, (str, Path)) else path_or_fileobj)
        position = raw.tell()
        magic = raw.read(6)
        raw.seek(position)
        if magic.startswith(b"\x1f\x8b"):
            decoded = stack.enter_context(gzip.GzipFile(fileobj=raw))
        elif magic.startswith(b"\xfd7zXZ\x00"):
            decoded = stack.enter_context(lzma.LZMAFile(raw))
        elif magic.startswith(b"BZh"):
            decoded = stack.enter_context(bz2.BZ2File(raw))
        elif magic.startswith(b"\x28\xb5\x2f\xfd"):
            # The admitted CPython runtime owns this decoder just as it owns
            # gzip/xz/bzip2; no executable discovery or alternate decoder lane.
            from compression.zstd import ZstdFile
            decoded = stack.enter_context(ZstdFile(raw))
        else:
            decoded = raw

        class BoundedReader:
            used = 0

            def read(self, size):
                remaining = maximum_expanded_bytes - self.used
                data = decoded.read(min(size, remaining + 1) if size >= 0 else remaining + 1)
                self.used += len(data)
                if self.used > maximum_expanded_bytes:
                    raise ValueError("archive decoded bytes exceed their expansion bound")
                return data

        headers, metadata_bytes = 0, 0

        class BoundedInfo(tarfile.TarInfo):
            # tarfile's member-processing extension point sees both ordinary
            # and recursive metadata headers before their bodies are allocated.
            # Overriding public frombuf alone does not guard Python 3.14 reads.
            def _proc_member(self, archive):
                nonlocal headers, metadata_bytes
                headers += 1
                if headers > MAX_ARCHIVE_ENTRIES:
                    raise ValueError("archive header count exceeds its expansion bound")
                metadata = (tarfile.XHDTYPE, tarfile.XGLTYPE, tarfile.SOLARIS_XHDTYPE,
                            tarfile.GNUTYPE_LONGNAME, tarfile.GNUTYPE_LONGLINK)
                if self.type in metadata:
                    metadata_bytes += self.size
                    if self.size > MAX_METADATA_BYTES or metadata_bytes > MAX_TOTAL_METADATA_BYTES:
                        raise ValueError("archive metadata exceeds its bound")
                if self.size < 0:
                    raise ValueError("archive has a negative member size")
                return super()._proc_member(archive)

            def _proc_sparse(self, *args):
                raise ValueError("sparse source archives are outside the selection contract")

            _proc_gnusparse_00 = _proc_sparse
            _proc_gnusparse_01 = _proc_sparse
            _proc_gnusparse_10 = _proc_sparse

        yield stack.enter_context(tarfile.open(fileobj=BoundedReader(), mode="r|",
                                               stream=True, tarinfo=BoundedInfo))


def read_members(path_or_fileobj, names: set[str], *,
                 maximum_member_bytes: int = MAX_FILE_BYTES,
                 maximum_selected_bytes: int = MAX_TOTAL_BYTES) -> dict[str, bytes]:
    """Read exact regular members, rejecting duplicates, links and missing names.

    Unselected upstream entries are never materialized. Full traversal catches a
    later duplicate and bounds expansion even when the selection appeared early.
    The caller verifies the archive's admitted size/hash before calling this.
    """
    if not names or len(names) > 1024:
        raise ValueError("archive selection must be finite and nonempty")
    for name in names:
        relative(name)
    selected, expanded, used = {}, 0, 0
    with open_archive(path_or_fileobj, maximum_expanded_bytes=MAX_EXPANDED_BYTES) as archive:
        for count, member in enumerate(archive, 1):
            expanded += member.size
            if (count > MAX_ARCHIVE_ENTRIES or member.size < 0 or
                    expanded > MAX_EXPANDED_BYTES):
                raise ValueError("archive expansion exceeds its bound")
            if member.name not in names:
                continue
            if member.name in selected or not member.isfile():
                raise ValueError("selected archive member is duplicate or not regular")
            used += member.size
            if member.size > maximum_member_bytes or used > maximum_selected_bytes:
                raise ValueError("selected archive bytes exceed their bound")
            with archive.extractfile(member) as stream:
                data = stream.read(member.size + 1)
            if len(data) != member.size:
                raise ValueError("selected archive member is truncated")
            selected[member.name] = data
    if selected.keys() != names:
        raise ValueError("archive is missing selected members")
    return selected
