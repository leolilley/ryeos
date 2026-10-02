"""Capture Modal's already-projected user driver from an existing guest process.

This entry never creates a Modal app/job, contacts a provider, or starts CUDA.
An authorized owner must invoke it inside the selected guest before any CUDA
or NVML library is otherwise loaded. The two explicit ``dlopen`` requests are
an unqualified observation, not a provider attestation or runtime qualification.
"""
from __future__ import annotations

import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import re
import stat
import struct
import sys
import secrets
from typing import Callable, Iterable, Mapping


SCHEMA = 'ryeos.modal_guest_driver_capture.v1'
REQUESTED_SONAMES = ('libcuda.so.1', 'libnvidia-ml.so.1')
CAPTURE_RELATIVE_ROOT = Path('inputs/modal-driver-capture')
MAX_CAPTURE_ENTRIES = 512
MAX_FILE_BYTES = 268_435_456
MAX_TOTAL_BYTES = 1_073_741_824
ELF_HEADER = struct.Struct('<16sHHIQQQIHHHHHH')
PROGRAM_HEADER = struct.Struct('<IIQQQQQQ')
DYNAMIC = struct.Struct('<qQ')
PT_LOAD, PT_DYNAMIC, ET_DYN, EM_X86_64 = 1, 2, 3, 62
DT_NULL, DT_NEEDED, DT_STRTAB, DT_STRSZ, DT_SONAME = 0, 1, 5, 10, 14
_MAPS_LINE = re.compile(
    r'^(?P<range>[0-9a-f]+-[0-9a-f]+)\s+'
    r'(?P<permissions>[rwxps-]{4})\s+'
    r'(?P<offset>[0-9a-f]+)\s+'
    r'(?P<device>[0-9a-f]+:[0-9a-f]+)\s+'
    r'(?P<inode>[0-9]+)\s*(?P<path>.*)$'
)


class CaptureRefused(ValueError):
    """The bounded guest observation was incomplete or unstable."""


_DIR_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW


def _open_absolute_directory(path: Path) -> int:
    descriptor = os.open('/', _DIR_FLAGS)
    try:
        for part in path.parts[1:]:
            child = os.open(part, _DIR_FLAGS, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
        return descriptor
    except BaseException:
        os.close(descriptor)
        raise


def _open_output_parent(path: Path, workspace_root: Path) -> tuple[int, str]:
    workspace = workspace_root.resolve(strict=True)
    target = Path(os.path.abspath(path))
    try:
        relative = target.relative_to(workspace)
    except ValueError as error:
        raise CaptureRefused('capture output escaped its selected workspace') from error
    if not relative.parts:
        raise CaptureRefused('capture output cannot replace the workspace root')
    descriptor = _open_absolute_directory(workspace)
    try:
        for part in relative.parts[:-1]:
            try:
                child = os.open(part, _DIR_FLAGS, dir_fd=descriptor)
            except FileNotFoundError:
                try:
                    os.mkdir(part, mode=0o700, dir_fd=descriptor)
                except FileExistsError:
                    pass
                child = os.open(part, _DIR_FLAGS, dir_fd=descriptor)
            os.close(descriptor)
            descriptor = child
        try:
            os.stat(relative.parts[-1], dir_fd=descriptor, follow_symlinks=False)
        except FileNotFoundError:
            return descriptor, relative.parts[-1]
        raise CaptureRefused('capture output root already exists')
    except CaptureRefused:
        os.close(descriptor)
        raise
    except OSError as error:
        os.close(descriptor)
        raise CaptureRefused('capture output parent is not a no-symlink directory') from error


def _rename_noreplace(parent_fd: int, source_name: str, target_name: str) -> None:
    renameat2 = getattr(ctypes.CDLL(None, use_errno=True), 'renameat2', None)
    if renameat2 is None:
        raise CaptureRefused('create-only capture retention is unavailable on this guest')
    renameat2.argtypes = (ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint)
    renameat2.restype = ctypes.c_int
    if renameat2(parent_fd, os.fsencode(source_name), parent_fd, os.fsencode(target_name), 1) != 0:
        code = ctypes.get_errno()
        if code in (17, 39):
            raise CaptureRefused('capture output root already exists')
        raise OSError(code, os.strerror(code))


def _write_at(directory_fd: int, name: str, data: bytes) -> tuple[int, int]:
    descriptor = os.open(name, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC,
                         0o600, dir_fd=directory_fd)
    try:
        view = memoryview(data)
        while view:
            count = os.write(descriptor, view)
            view = view[count:]
        info = os.fstat(descriptor)
        return info.st_dev, info.st_ino
    finally:
        os.close(descriptor)


def _new_stage(parent_fd: int) -> tuple[str, int, tuple[int, int]]:
    for _ in range(8):
        name = f'.modal-driver-capture-{secrets.token_hex(16)}'
        try:
            os.mkdir(name, mode=0o700, dir_fd=parent_fd)
        except FileExistsError:
            continue
        descriptor = os.open(name, _DIR_FLAGS, dir_fd=parent_fd)
        info = os.fstat(descriptor)
        return name, descriptor, (info.st_dev, info.st_ino)
    raise CaptureRefused('private capture staging directory could not be created')


def _verify_stage(parent_fd: int, name: str, stage_fd: int, identity: tuple[int, int]) -> None:
    held = os.fstat(stage_fd)
    try:
        named = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
    except OSError as error:
        raise CaptureRefused('private capture staging directory was moved') from error
    if (stat.S_ISLNK(named.st_mode) or not stat.S_ISDIR(named.st_mode)
            or (named.st_dev, named.st_ino) != identity
            or (held.st_dev, held.st_ino) != identity):
        raise CaptureRefused('private capture staging directory identity changed')


def _verify_child(parent_fd: int, name: str, child_fd: int, identity: tuple[int, int]) -> None:
    held = os.fstat(child_fd)
    try:
        named = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
    except OSError as error:
        raise CaptureRefused(f'private capture child directory was moved: {name}') from error
    if (stat.S_ISLNK(named.st_mode) or not stat.S_ISDIR(named.st_mode)
            or (named.st_dev, named.st_ino) != identity
            or (held.st_dev, held.st_ino) != identity):
        raise CaptureRefused(f'private capture child directory identity changed: {name}')


def _verify_inventory(directory_fd: int, expected: Mapping[str, tuple[int, int]]) -> None:
    if set(os.listdir(directory_fd)) != set(expected):
        raise CaptureRefused('private capture staging inventory changed')
    for name, identity in expected.items():
        info = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
        if (not stat.S_ISREG(info.st_mode) or (info.st_dev, info.st_ino) != identity):
            raise CaptureRefused(f'private capture staging member identity changed: {name}')


def _verify_layout(
    directory_fd: int,
    *,
    files: Mapping[str, tuple[int, int]],
    directories: Mapping[str, tuple[int, int]],
) -> None:
    if set(os.listdir(directory_fd)) != set(files) | set(directories):
        raise CaptureRefused('private capture staging child inventory changed')
    for name, identity in files.items():
        info = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
        if (not stat.S_ISREG(info.st_mode) or (info.st_dev, info.st_ino) != identity):
            raise CaptureRefused(f'private capture staging member identity changed: {name}')
    for name, identity in directories.items():
        info = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
        if (not stat.S_ISDIR(info.st_mode) or (info.st_dev, info.st_ino) != identity):
            raise CaptureRefused(f'private capture staging directory changed: {name}')


def _cstring(table: bytes, offset: int) -> str:
    if type(offset) is not int or offset < 0 or offset >= len(table):
        raise CaptureRefused('ELF string offset is outside its table')
    end = table.find(b'\0', offset)
    if end < 0:
        raise CaptureRefused('ELF dynamic string is unterminated')
    try:
        value = table[offset:end].decode('ascii')
    except UnicodeDecodeError as error:
        raise CaptureRefused('ELF dynamic string is not ASCII') from error
    if not value or value in ('.', '..') or '/' in value or '\\' in value:
        raise CaptureRefused('ELF dynamic string is not a SONAME')
    return value


def _elf_metadata(data: bytes) -> tuple[str, tuple[str, ...]]:
    """Read bounded SONAME/NEEDED metadata from one ELF64 x86_64 DSO."""
    if len(data) < ELF_HEADER.size:
        raise CaptureRefused('mapped library has a truncated ELF header')
    header = ELF_HEADER.unpack_from(data)
    ident = header[0]
    if ident[:7] != b'\x7fELF\x02\x01\x01' or header[1:4] != (ET_DYN, EM_X86_64, 1):
        raise CaptureRefused('mapped driver dependency is not ELF64 x86_64')
    phoff, phentsize, phnum = header[5], header[9], header[10]
    if phentsize != PROGRAM_HEADER.size or not phnum or phnum > 1024:
        raise CaptureRefused('mapped library has an unsupported program-header table')
    phsize = phentsize * phnum
    if phoff < ELF_HEADER.size or phoff + phsize > len(data):
        raise CaptureRefused('mapped library has an out-of-file program-header table')
    loads: list[tuple[int, int, int, int]] = []
    dynamic: tuple[int, int] | None = None
    for index in range(phnum):
        item = PROGRAM_HEADER.unpack_from(data, phoff + index * phentsize)
        kind, file_offset, virtual, file_size, memory_size = item[0], item[2], item[3], item[5], item[6]
        if file_offset + file_size > len(data) or file_size > memory_size:
            raise CaptureRefused('mapped library has an invalid ELF segment')
        if kind == PT_LOAD:
            loads.append((virtual, file_offset, file_size, memory_size))
        elif kind == PT_DYNAMIC:
            if dynamic is not None:
                raise CaptureRefused('mapped library has multiple PT_DYNAMIC segments')
            dynamic = (file_offset, file_size)
    if dynamic is None:
        raise CaptureRefused('mapped library has no PT_DYNAMIC segment')
    dynamic_offset, dynamic_size = dynamic
    if not dynamic_size or dynamic_size % DYNAMIC.size or dynamic_size > len(data):
        raise CaptureRefused('mapped library has an invalid PT_DYNAMIC size')
    values: list[tuple[int, int]] = []
    for offset in range(dynamic_offset, dynamic_offset + dynamic_size, DYNAMIC.size):
        tag, value = DYNAMIC.unpack_from(data, offset)
        if tag == DT_NULL:
            break
        values.append((tag, value))
    string_address = next((value for tag, value in values if tag == DT_STRTAB), None)
    string_size = next((value for tag, value in values if tag == DT_STRSZ), None)
    if type(string_address) is not int or type(string_size) is not int or not 0 < string_size <= MAX_FILE_BYTES:
        raise CaptureRefused('mapped library has no bounded dynamic string table')
    offsets = []
    for virtual, file_offset, file_size, memory_size in loads:
        if virtual <= string_address and string_address + string_size <= virtual + file_size:
            offsets.append(file_offset + string_address - virtual)
    if len(offsets) != 1 or offsets[0] + string_size > len(data):
        raise CaptureRefused('mapped library dynamic strings have no unique file mapping')
    strings = data[offsets[0]:offsets[0] + string_size]
    soname_offset = next((value for tag, value in values if tag == DT_SONAME), None)
    if type(soname_offset) is not int:
        raise CaptureRefused('mapped library has no DT_SONAME')
    soname = _cstring(strings, soname_offset)
    needed = tuple(_cstring(strings, value) for tag, value in values if tag == DT_NEEDED)
    if len(needed) > 4096:
        raise CaptureRefused('mapped library has too many DT_NEEDED entries')
    return soname, needed


def _parse_maps(text: str) -> dict[tuple[int, int, int], dict[str, object]]:
    """Collapse /proc/self/maps rows to exact file identities."""
    result: dict[tuple[int, int, int], dict[str, object]] = {}
    for line in text.splitlines():
        match = _MAPS_LINE.match(line)
        if match is None:
            continue
        path = match.group('path')
        inode = int(match.group('inode'))
        if not path.startswith('/') or path.endswith(' (deleted)') or inode == 0:
            continue
        major_text, minor_text = match.group('device').split(':', 1)
        identity = (int(major_text, 16), int(minor_text, 16), inode)
        item = result.get(identity)
        if item is None:
            item = {'device_major': identity[0], 'device_minor': identity[1],
                    'inode': inode, 'path': path, 'ranges': []}
            result[identity] = item
        elif item['path'] != path:
            raise CaptureRefused('one mapped file identity has multiple guest paths')
        item['ranges'].append(match.group('range'))
    return result


def _snapshot_maps() -> dict[tuple[int, int, int], dict[str, object]]:
    try:
        text = Path('/proc/self/maps').read_text(encoding='utf-8')
    except OSError as error:
        raise CaptureRefused('cannot inspect the selected guest process maps') from error
    return _parse_maps(text)


def _stable_read(
    item: Mapping[str, object], *, maximum_bytes: int = MAX_FILE_BYTES,
) -> tuple[bytes, dict[str, object]]:
    path = Path(str(item['path']))
    try:
        descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    except OSError as error:
        raise CaptureRefused('mapped library is not readable without following a path') from error
    try:
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode):
            raise CaptureRefused('mapped library path is not a regular file')
        expected = (int(item['device_major']), int(item['device_minor']), int(item['inode']))
        observed = (os.major(before.st_dev), os.minor(before.st_dev), before.st_ino)
        if observed != expected:
            raise CaptureRefused('mapped library changed identity after load')
        if before.st_size <= 0 or before.st_size > min(MAX_FILE_BYTES, maximum_bytes):
            raise CaptureRefused('mapped library exceeds the remaining capture byte bound')
        chunks: list[bytes] = []
        total = 0
        while True:
            chunk = os.read(descriptor, min(1024 * 1024, maximum_bytes + 1 - total))
            if not chunk:
                break
            chunks.append(chunk)
            total += len(chunk)
            if total > maximum_bytes or total > MAX_FILE_BYTES:
                raise CaptureRefused('mapped library exceeds its remaining capture byte bound')
        after = os.fstat(descriptor)
    finally:
        os.close(descriptor)
    identity_before = (before.st_dev, before.st_ino, before.st_mode, before.st_size,
                       before.st_mtime_ns, before.st_ctime_ns)
    identity_after = (after.st_dev, after.st_ino, after.st_mode, after.st_size,
                      after.st_mtime_ns, after.st_ctime_ns)
    data = b''.join(chunks)
    if identity_before != identity_after or len(data) != before.st_size:
        raise CaptureRefused('mapped library changed while it was retained')
    return data, {'size_bytes': len(data), 'device_major': os.major(before.st_dev),
                  'device_minor': os.minor(before.st_dev), 'inode': before.st_ino,
                  'mtime_ns': before.st_mtime_ns, 'ctime_ns': before.st_ctime_ns}


def _symlink_edges(path_text: str) -> list[dict[str, str]]:
    """Record any guest path symlink edges visible for a mapped file."""
    path = Path(path_text)
    if not path.is_absolute():
        raise CaptureRefused('mapped guest path is not absolute')
    current = Path(path.anchor)
    edges: list[dict[str, str]] = []
    for part in path.parts[1:]:
        current = current / part
        try:
            if current.is_symlink():
                edges.append({'path': str(current), 'target': os.readlink(current)})
        except OSError as error:
            raise CaptureRefused('mapped path symlink edge could not be inspected') from error
    if len(edges) > 64:
        raise CaptureRefused('mapped path symlink chain exceeds its bound')
    return edges


def _matching_mapped_soname(soname: str, files: Mapping[tuple[int, int, int], Mapping[str, object]],
                            metadata: Mapping[tuple[int, int, int], tuple[str, tuple[str, ...]]]) -> tuple[int, int, int]:
    matches = [identity for identity, value in metadata.items() if value[0] == soname]
    if len(matches) != 1:
        raise CaptureRefused(f'{soname} has no unique mapped provider after initial load')
    return matches[0]


def _capture_projection_inner(
    *,
    output_parent_fd: int,
    output_name: str,
    load_library: Callable[[str], object] | None = None,
    snapshot: Callable[[], dict[tuple[int, int, int], dict[str, object]]] = _snapshot_maps,
    guest_image: str | None = None,
    maximum_entries: int = MAX_CAPTURE_ENTRIES,
    maximum_total_bytes: int = MAX_TOTAL_BYTES,
) -> dict[str, object]:
    """Load exactly two entry SONAMEs and retain their newly mapped closure."""
    if maximum_entries < 2 or maximum_entries > MAX_CAPTURE_ENTRIES:
        raise CaptureRefused('capture entry bound is invalid')
    if maximum_total_bytes < 1 or maximum_total_bytes > MAX_TOTAL_BYTES:
        raise CaptureRefused('capture total-byte bound is invalid')
    before = snapshot()
    if not isinstance(before, dict):
        raise CaptureRefused('guest map snapshot has an invalid shape')
    handles = []
    if load_library is None:
        load_library = lambda soname: ctypes.CDLL(
            soname, mode=getattr(os, 'RTLD_NOW', 2) | getattr(os, 'RTLD_LOCAL', 0)
        )
    try:
        for soname in REQUESTED_SONAMES:
            handles.append(load_library(soname))
    except Exception as error:
        raise CaptureRefused('fixed initial driver load set could not be retained') from error
    after = snapshot()
    if not isinstance(after, dict):
        raise CaptureRefused('guest map snapshot has an invalid shape')
    new_files = {identity: item for identity, item in after.items() if identity not in before}
    all_files = dict(before)
    all_files.update(after)
    if len(all_files) > MAX_CAPTURE_ENTRIES:
        raise CaptureRefused('guest mapped-file inventory exceeds its entry bound')

    data_by_identity: dict[tuple[int, int, int], bytes] = {}
    metadata: dict[tuple[int, int, int], tuple[str, tuple[str, ...]]] = {}
    stat_by_identity: dict[tuple[int, int, int], dict[str, object]] = {}
    total_read = 0
    for identity, item in all_files.items():
        remaining_bytes = maximum_total_bytes - total_read
        if remaining_bytes <= 0:
            raise CaptureRefused('mapped-file inspection exceeds total-byte bound')
        data, stable = _stable_read(
            item, maximum_bytes=min(MAX_FILE_BYTES, remaining_bytes),
        )
        total_read += len(data)
        if total_read > maximum_total_bytes:
            raise CaptureRefused('mapped-file inspection exceeds total-byte bound')
        try:
            soname, needed = _elf_metadata(data)
        except CaptureRefused:
            # A mapped non-ELF object cannot be a DT_NEEDED provider. It still
            # consumes the bounded inspection budget above.
            continue
        data_by_identity[identity] = data
        metadata[identity] = (soname, needed)
        stat_by_identity[identity] = stable

    entry_ids = {name: _matching_mapped_soname(name, all_files, metadata)
                 for name in REQUESTED_SONAMES}
    if any(identity in before for identity in entry_ids.values()):
        raise CaptureRefused('entry SONAME was already mapped before the initial observation')

    selected: list[tuple[int, int, int]] = []
    visited_sonames: set[str] = set()

    def visit(soname: str) -> None:
        if soname in visited_sonames:
            return
        matches = [identity for identity, value in metadata.items() if value[0] == soname]
        if len(matches) != 1:
            raise CaptureRefused(f'{soname} dependency is absent or ambiguous in guest mappings')
        identity = matches[0]
        visited_sonames.add(soname)
        selected.append(identity)
        for needed in metadata[identity][1]:
            visit(needed)

    for entry in REQUESTED_SONAMES:
        visit(entry)
    if len(selected) > maximum_entries:
        raise CaptureRefused('initial driver closure exceeds entry bound')
    total_selected = sum(len(data_by_identity[item]) for item in selected)
    if total_selected > maximum_total_bytes:
        raise CaptureRefused('initial driver closure exceeds total-byte bound')

    stage_name, stage_fd, stage_identity = _new_stage(output_parent_fd)
    files_fd = -1
    files_identity: tuple[int, int] | None = None
    files_written: dict[str, tuple[int, int]] = {}
    root_written: dict[str, tuple[int, int]] = {}
    try:
        os.mkdir('files', mode=0o700, dir_fd=stage_fd)
        files_fd = os.open('files', _DIR_FLAGS, dir_fd=stage_fd)
        files_info = os.fstat(files_fd)
        files_identity = (files_info.st_dev, files_info.st_ino)
        records: list[dict[str, object]] = []
        for identity in selected:
            item = all_files[identity]
            soname, needed = metadata[identity]
            data = data_by_identity[identity]
            digest = hashlib.sha256(data).hexdigest()
            retained = identity in new_files
            relative_path = f'files/{digest}.so' if retained else None
            if relative_path is not None:
                files_written[f'{digest}.so'] = _write_at(files_fd, f'{digest}.so', data)
            records.append({
                'soname': soname,
                'needed': list(needed),
                'relative_path': relative_path,
                'sha256': digest,
                'mapped_path': str(item['path']),
                'symlink_edges': _symlink_edges(str(item['path'])),
                'mapped_identity': stat_by_identity[identity],
                'newly_mapped_by_initial_load': retained,
                'loader_name_observation': soname,
            })

        loader_paths = [str(item['path']) for item in after.values()
                        if Path(str(item['path'])).name.startswith(('ld-linux-', 'ld-musl-'))]
        if len(set(loader_paths)) != 1:
            raise CaptureRefused('selected guest loader identity is absent or ambiguous')
        record = {
            'schema': SCHEMA,
            'coverage': 'initial_load_only_unqualified',
            'guest_image': guest_image,
            'loader_path': loader_paths[0],
            'driver_api_version': None,
            'requested_sonames': list(REQUESTED_SONAMES),
            'loaded_entry_sonames': list(REQUESTED_SONAMES),
            'files': records,
            'limits': {'maximum_entries': maximum_entries,
                       'maximum_file_bytes': MAX_FILE_BYTES,
                       'maximum_total_bytes': maximum_total_bytes},
        }
        root_written['capture.json'] = _write_at(
            stage_fd, 'capture.json',
            (json.dumps(record, sort_keys=True, separators=(',', ':')) + '\n').encode(),
        )
        # Keep handles alive through the final record write so lazy close cannot
        # invalidate the observed mapping during capture.
        if len(handles) != len(REQUESTED_SONAMES):
            raise CaptureRefused('fixed initial load set was not retained')
        _verify_stage(output_parent_fd, stage_name, stage_fd, stage_identity)
        _verify_child(stage_fd, 'files', files_fd, files_identity)
        _verify_layout(stage_fd, files=root_written, directories={'files': files_identity})
        _verify_inventory(files_fd, files_written)
        _rename_noreplace(output_parent_fd, stage_name, output_name)
        _verify_stage(output_parent_fd, output_name, stage_fd, stage_identity)
        _verify_child(stage_fd, 'files', files_fd, files_identity)
        _verify_layout(stage_fd, files=root_written, directories={'files': files_identity})
        _verify_inventory(files_fd, files_written)
    except CaptureRefused:
        # Leave the bounded private stage quarantined for its workspace owner.
        # Name-based deletion is unsafe without writer exclusion.
        raise
    except OSError as error:
        # Leave the bounded private stage quarantined for its workspace owner.
        # Name-based deletion is unsafe without writer exclusion.
        raise CaptureRefused('capture output could not be retained atomically') from error
    finally:
        if files_fd >= 0:
            os.close(files_fd)
        os.close(stage_fd)
    return record


def capture_projection(
    output_root: Path,
    *,
    load_library: Callable[[str], object] | None = None,
    snapshot: Callable[[], dict[tuple[int, int, int], dict[str, object]]] = _snapshot_maps,
    guest_image: str | None = None,
    maximum_entries: int = MAX_CAPTURE_ENTRIES,
    maximum_total_bytes: int = MAX_TOTAL_BYTES,
    workspace_root: Path | None = None,
) -> dict[str, object]:
    workspace_root = Path.cwd() if workspace_root is None else Path(workspace_root)
    parent_fd, output_name = _open_output_parent(Path(output_root), workspace_root)
    try:
        return _capture_projection_inner(
            output_parent_fd=parent_fd, output_name=output_name,
            load_library=load_library, snapshot=snapshot, guest_image=guest_image,
            maximum_entries=maximum_entries, maximum_total_bytes=maximum_total_bytes,
        )
    finally:
        os.close(parent_fd)


def main(argv: Iterable[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', default=CAPTURE_RELATIVE_ROOT.as_posix())
    args = parser.parse_args(argv)
    relative = Path(args.output)
    if relative.is_absolute() or '..' in relative.parts or relative != CAPTURE_RELATIVE_ROOT:
        raise CaptureRefused('capture output must use the signed relative guest workspace root')
    image = os.environ.get('MODAL_IMAGE_ID')
    record = capture_projection(Path.cwd() / relative, guest_image=image)
    print(json.dumps({'schema': SCHEMA, 'requested_sonames': record['requested_sonames'],
                      'file_count': len(record['files'])}, sort_keys=True))
    return 0


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except CaptureRefused as error:
        print(f'capture refused: {error}', file=sys.stderr)
        raise SystemExit(2)
