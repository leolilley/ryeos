"""Resolve the selected CUDA driver's ELF dependencies inside admitted roots.

This is a source-side closure check. It does not qualify symbol versions, load
the driver, attest mapped providers, or establish device identity.
"""
from __future__ import annotations

from dataclasses import dataclass
import os
from pathlib import Path
import struct
from types import MappingProxyType
from typing import Mapping, Sequence


_ELF_HEADER = struct.Struct('<16sHHIQQQIHHHHHH')
_PROGRAM_HEADER = struct.Struct('<IIQQQQQQ')
_DYNAMIC = struct.Struct('<qQ')
_PT_LOAD = 1
_PT_DYNAMIC = 2
_ET_DYN = 3
_EM_X86_64 = 62
_DT_NULL = 0
_DT_NEEDED = 1
_DT_STRTAB = 5
_DT_STRSZ = 10
_DT_SONAME = 14
_DT_RPATH = 15
_DT_RUNPATH = 29


class DriverClosureError(ValueError):
    """A selected driver has missing, ambiguous, or escaping ELF inputs."""


@dataclass(frozen=True)
class TreeBounds:
    """Bounds copied from an admitted tree relationship; there are no defaults."""

    maximum_entries: int
    maximum_depth: int
    maximum_file_bytes: int
    maximum_total_bytes: int

    def __post_init__(self) -> None:
        if any(type(member) is not int or member <= 0 for member in (
            self.maximum_entries, self.maximum_depth, self.maximum_file_bytes,
            self.maximum_total_bytes,
        )):
            raise DriverClosureError('admitted driver tree bounds must be positive integers')
        if self.maximum_file_bytes > self.maximum_total_bytes:
            raise DriverClosureError('admitted file bound exceeds its tree byte bound')

    @classmethod
    def from_contract(cls, value: Mapping[str, object]) -> 'TreeBounds':
        keys = ('maximum_entries', 'maximum_depth', 'maximum_file_bytes', 'maximum_total_bytes')
        if not isinstance(value, Mapping) or set(value) != set(keys):
            raise DriverClosureError('driver closure requires exact admitted tree bounds')
        members = tuple(value[key] for key in keys)
        if any(type(member) is not int or member <= 0 for member in members):
            raise DriverClosureError('admitted driver tree bounds must be positive integers')
        return cls(*members)


@dataclass(frozen=True)
class _DynamicInfo:
    soname: str | None
    needed: tuple[str, ...]
    rpath: str | None
    runpath: str | None


@dataclass(frozen=True)
class DriverClosure:
    """Exact dependency-first providers for one selected libcuda entry."""

    providers: Mapping[str, Path]
    load_order: tuple[Path, ...]


def _within(root: Path, candidate: Path) -> bool:
    return candidate == root or root in candidate.parents


def _c_string(data: bytes, offset: int) -> str:
    if offset < 0 or offset >= len(data):
        raise DriverClosureError('ELF dynamic string offset is outside its table')
    end = data.find(b'\0', offset)
    if end < 0:
        raise DriverClosureError('ELF dynamic string is unterminated')
    try:
        value = data[offset:end].decode('ascii')
    except UnicodeDecodeError as error:
        raise DriverClosureError('ELF dynamic string is not ASCII') from error
    if not value:
        raise DriverClosureError('ELF dynamic string is empty')
    return value


def _file_translation(
    virtual: int,
    length: int,
    loads: tuple[tuple[int, int, int, int], ...],
    total_file_bytes: int,
    name: str,
) -> int:
    end = virtual + length
    offsets = []
    for load_virtual, file_offset, load_file_bytes, memory_size in loads:
        memory_end = load_virtual + memory_size
        if virtual >= memory_end or load_virtual >= end:
            continue
        file_end = load_virtual + load_file_bytes
        if virtual < load_virtual or end > file_end:
            raise DriverClosureError(f'{name} overlaps a partial or zero-filled PT_LOAD range')
        offsets.append(file_offset + virtual - load_virtual)
    if len(offsets) != 1:
        raise DriverClosureError(f'{name} has missing or ambiguous PT_LOAD file mapping')
    offset = offsets[0]
    if offset < 0 or offset + length > total_file_bytes:
        raise DriverClosureError(f'{name} PT_LOAD mapping is outside the file')
    return offset


def _dynamic_info(path: Path, maximum_file_bytes: int) -> _DynamicInfo:
    try:
        with path.open('rb') as source:
            size = os.fstat(source.fileno()).st_size
            if size > maximum_file_bytes:
                raise DriverClosureError(f'{path.name} exceeds its admitted file-byte bound')
            header_bytes = source.read(_ELF_HEADER.size)
            if len(header_bytes) != _ELF_HEADER.size:
                raise DriverClosureError(f'{path.name} has a truncated ELF header')
            header = _ELF_HEADER.unpack(header_bytes)
            ident = header[0]
            if ident[:4] != b'\x7fELF' or ident[4:7] != b'\x02\x01\x01':
                raise DriverClosureError(f'{path.name} is not ELF64 little-endian v1')
            if header[1] != _ET_DYN or header[2] != _EM_X86_64 or header[3] != 1:
                raise DriverClosureError(f'{path.name} is not an x86_64 shared object')
            phoff, phentsize, phnum = header[5], header[9], header[10]
            if phentsize != _PROGRAM_HEADER.size or not phnum:
                raise DriverClosureError(f'{path.name} has an unsupported program-header table')
            phbytes = phentsize * phnum
            if (phbytes > maximum_file_bytes or phoff < _ELF_HEADER.size
                    or phoff + phbytes > size):
                raise DriverClosureError(f'{path.name} has an out-of-file program-header table')
            source.seek(phoff)
            encoded_headers = source.read(phbytes)
            if len(encoded_headers) != phbytes:
                raise DriverClosureError(f'{path.name} has a truncated program-header table')
            loads: list[tuple[int, int, int, int]] = []
            dynamic: tuple[int, int, int] | None = None
            for offset in range(0, phbytes, phentsize):
                item = _PROGRAM_HEADER.unpack_from(encoded_headers, offset)
                kind, file_offset, virtual, file_size, memory_size = (
                    item[0], item[2], item[3], item[5], item[6]
                )
                if file_offset + file_size > size:
                    raise DriverClosureError(f'{path.name} has an out-of-file ELF segment')
                if file_size > memory_size:
                    raise DriverClosureError(f'{path.name} file-backed segment exceeds memory size')
                if kind == _PT_LOAD:
                    loads.append((virtual, file_offset, file_size, memory_size))
                elif kind == _PT_DYNAMIC:
                    if dynamic is not None:
                        raise DriverClosureError(f'{path.name} has multiple PT_DYNAMIC segments')
                    dynamic = (file_offset, virtual, file_size)
            if dynamic is None:
                raise DriverClosureError(f'{path.name} has no PT_DYNAMIC segment')
            dynamic_offset, dynamic_virtual, dynamic_size = dynamic
            mapped_dynamic = _file_translation(
                dynamic_virtual, dynamic_size, tuple(loads), size, f'{path.name} PT_DYNAMIC'
            )
            if mapped_dynamic != dynamic_offset:
                raise DriverClosureError(f'{path.name} PT_DYNAMIC is not covered by PT_LOAD')
            if (dynamic_size == 0 or dynamic_size > maximum_file_bytes
                    or dynamic_size % _DYNAMIC.size):
                raise DriverClosureError(f'{path.name} has an invalid PT_DYNAMIC size')
            source.seek(dynamic_offset)
            encoded_dynamic = source.read(dynamic_size)
            if len(encoded_dynamic) != dynamic_size:
                raise DriverClosureError(f'{path.name} has a truncated PT_DYNAMIC segment')
            entries: list[tuple[int, int]] = []
            for offset in range(0, dynamic_size, _DYNAMIC.size):
                entry = _DYNAMIC.unpack_from(encoded_dynamic, offset)
                if entry[0] == _DT_NULL:
                    break
                entries.append(entry)
            else:
                raise DriverClosureError(f'{path.name} has no DT_NULL terminator')
            string_tables = [value for tag, value in entries if tag == _DT_STRTAB]
            string_sizes = [value for tag, value in entries if tag == _DT_STRSZ]
            if len(string_tables) != 1 or len(string_sizes) != 1:
                raise DriverClosureError(f'{path.name} has duplicate or missing dynamic string metadata')
            strings_va, strings_size = string_tables[0], string_sizes[0]
            if strings_size == 0 or strings_size > maximum_file_bytes:
                raise DriverClosureError(f'{path.name} has no bounded dynamic string table')
            strings_offset = _file_translation(
                strings_va, strings_size, tuple(loads), size, f'{path.name} dynamic strings'
            )
            source.seek(strings_offset)
            strings = source.read(strings_size)
            if len(strings) != strings_size:
                raise DriverClosureError(f'{path.name} has a truncated dynamic string table')
            sonames = [_c_string(strings, value) for tag, value in entries if tag == _DT_SONAME]
            if len(sonames) > 1:
                raise DriverClosureError(f'{path.name} has duplicate DT_SONAME entries')
            needed = tuple(_c_string(strings, value) for tag, value in entries if tag == _DT_NEEDED)
            if (len(needed) > maximum_file_bytes // _DYNAMIC.size
                    or len(set(needed)) != len(needed)):
                raise DriverClosureError(f'{path.name} has duplicate DT_NEEDED entries')
            if any('/' in name or name in ('.', '..') for name in needed):
                raise DriverClosureError(f'{path.name} has a path-bearing DT_NEEDED entry')
            rpaths = [_c_string(strings, value) for tag, value in entries if tag == _DT_RPATH]
            runpaths = [_c_string(strings, value) for tag, value in entries if tag == _DT_RUNPATH]
            if len(rpaths) > 1 or len(runpaths) > 1 or (rpaths and runpaths):
                raise DriverClosureError(f'{path.name} has duplicate or mixed RPATH/RUNPATH entries')
            if rpaths:
                raise DriverClosureError(f'{path.name} uses unsupported inherited DT_RPATH')
            if sonames and ('/' in sonames[0] or sonames[0] in ('.', '..')):
                raise DriverClosureError(f'{path.name} has a path-bearing DT_SONAME')
            return _DynamicInfo(
                sonames[0] if sonames else None, needed,
                rpaths[0] if rpaths else None, runpaths[0] if runpaths else None,
            )
    except (OSError, struct.error) as error:
        raise DriverClosureError(f'cannot inspect selected ELF member {path.name}') from error


def _expand_search_path(encoded: str, origin: Path, roots: tuple[Path, ...]) -> tuple[Path, ...]:
    result: list[Path] = []
    for entry in encoded.split(':'):
        if not entry:
            raise DriverClosureError('empty ELF RPATH/RUNPATH entry is forbidden')
        expanded = entry.replace('${ORIGIN}', str(origin)).replace('$ORIGIN', str(origin))
        if '$' in expanded:
            raise DriverClosureError('unsupported ELF RPATH/RUNPATH variable')
        path = Path(expanded)
        if not path.is_absolute():
            raise DriverClosureError('relative ELF RPATH/RUNPATH entry is forbidden')
        try:
            resolved = path.resolve(strict=True)
        except OSError as error:
            raise DriverClosureError('ELF RPATH/RUNPATH directory is absent') from error
        if not resolved.is_dir() or not any(_within(root, resolved) for root in roots):
            raise DriverClosureError('ELF RPATH/RUNPATH escapes selected library roots')
        result.append(resolved)
    return tuple(result)


def inspect_cuda_driver_closure(
    gnu_library_root: Path,
    driver_root: Path,
    *,
    gnu_bounds: TreeBounds,
    driver_bounds: TreeBounds,
    entry_sonames: Sequence[str] = ('libcuda.so.1',),
) -> DriverClosure:
    """Require each selected driver entry's NEEDED graph to resolve in selected roots.

    ``gnu_library_root`` is the already selected GNU ``python/lib`` directory;
    ``driver_root`` is the selected driver's ``lib`` directory. The caller still
    needs destination qualification for symbol versions, actual mappings, ABI,
    and physical device identity.
    """
    required_entries = tuple(entry_sonames)
    if (not required_entries or len(set(required_entries)) != len(required_entries)
            or any(not isinstance(name, str) or not name.endswith('.so.1')
                   or '/' in name or '\\' in name or name in ('.so.1', '..so.1')
                   for name in required_entries)):
        raise DriverClosureError('required driver entry SONAME set is invalid')
    roots = tuple(path.resolve(strict=True) for path in (gnu_library_root, driver_root))
    if (any(not root.is_dir() for root in roots) or roots[0] == roots[1]
            or _within(roots[0], roots[1]) or _within(roots[1], roots[0])):
        raise DriverClosureError('GNU and driver library roots must be separate directories')
    bounds_by_root = {roots[0]: gnu_bounds, roots[1]: driver_bounds}
    for root, bounds in bounds_by_root.items():
        entries = 0
        total_bytes = 0
        for member in root.rglob('*'):
            entries += 1
            if entries > bounds.maximum_entries or len(member.relative_to(root).parts) > bounds.maximum_depth:
                raise DriverClosureError(f'selected tree exceeds admitted entry/depth bounds: {root.name}')
            if member.is_symlink():
                try:
                    target = member.resolve(strict=True)
                except OSError as error:
                    raise DriverClosureError(f'dangling selected library member {member.name}') from error
                if not _within(root, target):
                    raise DriverClosureError(f'selected library symlink escaped root: {member.name}')
                if target.is_dir():
                    raise DriverClosureError(f'selected library directory symlink is unsupported: {member.name}')
            elif member.is_file():
                size = member.stat().st_size
                if size > bounds.maximum_file_bytes:
                    raise DriverClosureError(f'selected file exceeds admitted bound: {member.name}')
                total_bytes += size
                if total_bytes > bounds.maximum_total_bytes:
                    raise DriverClosureError(f'selected tree exceeds admitted byte bound: {root.name}')
    index: dict[str, Path] = {}
    info_by_path: dict[Path, _DynamicInfo] = {}
    inode_paths: set[Path] = set()
    for root in roots:
        for candidate in root.rglob('*'):
            if '.so' not in candidate.name:
                continue
            try:
                resolved = candidate.resolve(strict=True)
            except OSError as error:
                raise DriverClosureError(f'dangling selected library member {candidate.name}') from error
            if not _within(root, resolved):
                raise DriverClosureError(f'selected library symlink escaped root: {candidate.name}')
            if not resolved.is_file() or resolved in inode_paths:
                continue
            inode_paths.add(resolved)
            info = _dynamic_info(resolved, bounds_by_root[root].maximum_file_bytes)
            info_by_path[resolved] = info
            if info.soname is None:
                continue
            previous = index.get(info.soname)
            if previous is not None and previous != resolved:
                raise DriverClosureError(f'ambiguous selected provider for {info.soname}')
            index[info.soname] = resolved

    entry_paths: dict[str, Path] = {}
    for soname in required_entries:
        try:
            entry_path = (roots[1] / soname).resolve(strict=True)
        except OSError as error:
            raise DriverClosureError(f'selected driver entry is missing: {soname}') from error
        if not _within(roots[1], entry_path) or not entry_path.is_file():
            raise DriverClosureError(f'selected {soname} escaped or is not a regular file')
        entry_info = info_by_path.get(entry_path)
        if entry_info is None:
            raise DriverClosureError('selected driver entry was not indexed under its product bounds')
        if entry_info.soname != soname:
            raise DriverClosureError(f'selected driver entry has unexpected DT_SONAME for {soname}')
        if index.get(soname) != entry_path:
            raise DriverClosureError(f'selected driver entry is ambiguous for {soname}')
        entry_paths[soname] = entry_path

    selected: dict[str, Path] = {}
    ordered: list[Path] = []
    visiting: set[str] = set()
    visited: set[str] = set()
    search_cache: dict[Path, tuple[Path, ...]] = {}
    maximum_dependency_nodes = gnu_bounds.maximum_entries + driver_bounds.maximum_entries
    maximum_dependency_edges = (
        gnu_bounds.maximum_total_bytes + driver_bounds.maximum_total_bytes
    ) // _DYNAMIC.size
    dependency_edges = 0

    def visit(soname: str) -> None:
        nonlocal dependency_edges
        if soname in visited:
            return
        if soname in visiting:
            raise DriverClosureError(f'cyclic selected ELF dependency at {soname}')
        provider = index.get(soname)
        if provider is None:
            raise DriverClosureError(f'missing selected driver dependency {soname}')
        visiting.add(soname)
        info = info_by_path[provider]
        if info.soname != soname:
            raise DriverClosureError(f'provider SONAME changed for {soname}')
        search_dirs = search_cache.get(provider)
        if search_dirs is None:
            search_dirs = (roots[0],) + (
                _expand_search_path(info.runpath, provider.parent, roots)
                if info.runpath else ()
            )
            search_cache[provider] = search_dirs
        for needed in info.needed:
            dependency_edges += 1
            if dependency_edges > maximum_dependency_edges:
                raise DriverClosureError('selected dependency graph exceeds admitted byte budget')
            child = index.get(needed)
            if child is None:
                raise DriverClosureError(f'missing selected driver dependency {needed}')
            if (needed not in visiting and needed not in visited
                    and len(visiting) + len(visited) >= maximum_dependency_nodes):
                raise DriverClosureError('selected dependency graph exceeds admitted entry budget')
            resolved = None
            for directory in search_dirs:
                candidate = directory / needed
                if candidate.is_symlink() or candidate.exists():
                    try:
                        resolved = candidate.resolve(strict=True)
                    except OSError as error:
                        raise DriverClosureError(
                            f'broken selected loader entry for {needed}'
                        ) from error
                    break
            if resolved is None:
                raise DriverClosureError(f'{needed} has no selected loader search entry')
            if not resolved.is_file() or not any(_within(root, resolved) for root in roots):
                raise DriverClosureError(f'{needed} loader search selected a path outside admitted roots')
            if resolved != child:
                raise DriverClosureError(
                    f'{needed} first loader entry differs from its unique indexed provider'
                )
            visit(needed)
        visiting.remove(soname)
        visited.add(soname)
        selected[soname] = provider
        ordered.append(provider)

    for soname in required_entries:
        visit(soname)
    providers = dict(selected)
    if 'libcuda.so.1' in entry_paths:
        providers['cuda'] = entry_paths['libcuda.so.1']
    if 'libnvidia-ml.so.1' in entry_paths:
        providers['nvml'] = entry_paths['libnvidia-ml.so.1']
    return DriverClosure(MappingProxyType(providers), tuple(ordered))
