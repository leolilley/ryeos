# ryeos-tool:
#   category: local-inference/modal-driver-normalize
#   name: normalize
#   version: "1.0.0"
#   description: Normalize an unqualified fixed-SONAME guest capture into a bounded immutable driver tree
#   executor_id: tool:local-inference/modal-driver-normalize/runtime
#   execution_protocol: protocol:ryeos/core/opaque
#   effects: live
#   filesystem_authority: captured_execution
#   network_authority: isolated
#   config_schema:
#     type: object
#     properties:
#       schema: {type: string, const: ryeos.modal_driver_normalizer_request.v1}
#       capture_root: {type: string, const: inputs/modal-driver-capture}
#       output_root: {type: string, const: products/modal-guest-user-driver}
#     required: [schema, capture_root, output_root]
#     additionalProperties: false
#   external_content:
#     - id: producer-python
#       kind: tree
#       mode: pinned
#       digest: 800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf
#       mount_root: execution_runtime
#       mount: producer-python
#   external_product_slots:
#     - id: gnu_runtime
#       relationship_ref: config:ryeos-runtime/gnu-cuda-products-draft
#       relationship: gnu_runtime_to_driver_normalizer
#       kind: tree
#       mount_root: execution_runtime
#       mount: python-gnu-private
"""Normalize the owner-staged guest capture using only signed workspace roots."""
from __future__ import annotations

import hashlib
import ctypes
import json
import os
from pathlib import Path
import re
import stat
import struct
import sys
import secrets
from typing import Mapping


REQUEST_SCHEMA = 'ryeos.modal_driver_normalizer_request.v1'
CAPTURE_SCHEMA = 'ryeos.modal_guest_driver_capture.v1'
PRODUCT_RECORD_SCHEMA = 'ryeos.modal_guest_driver_product_record.v1'
REQUESTED_SONAMES = ('libcuda.so.1', 'libnvidia-ml.so.1')
PRODUCER_PYTHON_HASH = '800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf'
MAX_ENTRIES = 10000
MAX_DEPTH = 64
MAX_FILE_BYTES = 134217728
MAX_TOTAL_BYTES = 268435456
ELF_HEADER = struct.Struct('<16sHHIQQQIHHHHHH')
PROGRAM_HEADER = struct.Struct('<IIQQQQQQ')
DYNAMIC = struct.Struct('<qQ')
PT_LOAD, PT_DYNAMIC, ET_DYN, EM_X86_64 = 1, 2, 3, 62
DT_NULL, DT_NEEDED, DT_STRTAB, DT_STRSZ, DT_SONAME = 0, 1, 5, 10, 14
_HASH = re.compile(r'[0-9a-f]{64}\Z')


class NormalizeRefused(ValueError):
    """The retained input, selected GNU tree, or output bounds are invalid."""


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


def _open_workspace_root(workspace_root: Path) -> int:
    workspace = workspace_root.resolve(strict=True)
    return _open_absolute_directory(workspace)


def _open_workspace_dir(workspace_fd: int, path: Path, workspace_root: Path) -> int:
    workspace = workspace_root.resolve(strict=True)
    target = Path(os.path.abspath(path))
    try:
        relative = target.relative_to(workspace)
    except ValueError as error:
        raise NormalizeRefused('workspace path escaped its selected root') from error
    if not relative.parts:
        raise NormalizeRefused('workspace root cannot be used as a product path')
    descriptor = os.dup(workspace_fd)
    try:
        for part in relative.parts:
            try:
                child = os.open(part, _DIR_FLAGS, dir_fd=descriptor)
            except OSError as error:
                raise NormalizeRefused('workspace input path is absent or contains a symlink') from error
            os.close(descriptor)
            descriptor = child
        return descriptor
    except BaseException:
        os.close(descriptor)
        raise


def _open_output_parent(workspace_fd: int, path: Path, workspace_root: Path) -> tuple[int, str]:
    workspace = workspace_root.resolve(strict=True)
    target = Path(os.path.abspath(path))
    try:
        relative = target.relative_to(workspace)
    except ValueError as error:
        raise NormalizeRefused('driver output escaped its selected workspace') from error
    if not relative.parts:
        raise NormalizeRefused('workspace root cannot be a driver output')
    descriptor = os.dup(workspace_fd)
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
        raise NormalizeRefused('driver output already exists')
    except NormalizeRefused:
        os.close(descriptor)
        raise
    except OSError as error:
        os.close(descriptor)
        raise NormalizeRefused('driver output parent is not a no-symlink directory') from error


def _read_regular_at(directory_fd: int, name: str, limit: int) -> bytes:
    try:
        descriptor = os.open(name, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW, dir_fd=directory_fd)
    except OSError as error:
        raise NormalizeRefused('captured workspace member is not an admitted regular file') from error
    try:
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode) or before.st_size <= 0 or before.st_size > limit:
            raise NormalizeRefused('captured workspace member exceeds its regular-file bound')
        chunks = []
        total = 0
        while True:
            chunk = os.read(descriptor, min(1024 * 1024, limit + 1 - total))
            if not chunk:
                break
            chunks.append(chunk)
            total += len(chunk)
            if total > limit:
                raise NormalizeRefused('captured workspace member exceeds its regular-file bound')
        after = os.fstat(descriptor)
    finally:
        os.close(descriptor)
    first = (before.st_dev, before.st_ino, before.st_mode, before.st_size,
             before.st_mtime_ns, before.st_ctime_ns)
    last = (after.st_dev, after.st_ino, after.st_mode, after.st_size,
            after.st_mtime_ns, after.st_ctime_ns)
    data = b''.join(chunks)
    if first != last or len(data) != before.st_size:
        raise NormalizeRefused('captured workspace member changed while it was read')
    return data


def _read_capture_member(capture_fd: int, relative: str, limit: int) -> bytes:
    parts = Path(relative).parts
    if len(parts) != 2 or parts[0] != 'files' or parts[1] in ('', '.', '..'):
        raise NormalizeRefused('guest capture member path is not canonical')
    try:
        files_fd = os.open('files', _DIR_FLAGS, dir_fd=capture_fd)
    except OSError as error:
        raise NormalizeRefused('guest capture files root is not a no-symlink directory') from error
    try:
        return _read_regular_at(files_fd, parts[1], limit)
    finally:
        os.close(files_fd)


def _rename_noreplace(parent_fd: int, source_name: str, target_name: str) -> None:
    renameat2 = getattr(ctypes.CDLL(None, use_errno=True), 'renameat2', None)
    if renameat2 is None:
        raise NormalizeRefused('create-only driver product retention is unavailable')
    renameat2.argtypes = (ctypes.c_int, ctypes.c_char_p, ctypes.c_int, ctypes.c_char_p, ctypes.c_uint)
    renameat2.restype = ctypes.c_int
    if renameat2(parent_fd, os.fsencode(source_name), parent_fd, os.fsencode(target_name), 1) != 0:
        code = ctypes.get_errno()
        if code in (17, 39):
            raise NormalizeRefused('driver output already exists')
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
        name = f'.modal-driver-product-{secrets.token_hex(16)}'
        try:
            os.mkdir(name, mode=0o700, dir_fd=parent_fd)
        except FileExistsError:
            continue
        descriptor = os.open(name, _DIR_FLAGS, dir_fd=parent_fd)
        info = os.fstat(descriptor)
        return name, descriptor, (info.st_dev, info.st_ino)
    raise NormalizeRefused('private driver staging directory could not be created')


def _verify_stage(parent_fd: int, name: str, stage_fd: int, identity: tuple[int, int]) -> None:
    held = os.fstat(stage_fd)
    try:
        named = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
    except OSError as error:
        raise NormalizeRefused('private driver staging directory was moved') from error
    if (stat.S_ISLNK(named.st_mode) or not stat.S_ISDIR(named.st_mode)
            or (named.st_dev, named.st_ino) != identity
            or (held.st_dev, held.st_ino) != identity):
        raise NormalizeRefused('private driver staging directory identity changed')


def _verify_child(parent_fd: int, name: str, child_fd: int, identity: tuple[int, int]) -> None:
    held = os.fstat(child_fd)
    try:
        named = os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
    except OSError as error:
        raise NormalizeRefused(f'private driver child directory was moved: {name}') from error
    if (stat.S_ISLNK(named.st_mode) or not stat.S_ISDIR(named.st_mode)
            or (named.st_dev, named.st_ino) != identity
            or (held.st_dev, held.st_ino) != identity):
        raise NormalizeRefused(f'private driver child directory identity changed: {name}')


def _verify_inventory(directory_fd: int, expected: Mapping[str, tuple[int, int]]) -> None:
    if set(os.listdir(directory_fd)) != set(expected):
        raise NormalizeRefused('private driver staging inventory changed')
    for name, identity in expected.items():
        info = os.stat(name, dir_fd=directory_fd, follow_symlinks=False)
        if (not stat.S_ISREG(info.st_mode) or (info.st_dev, info.st_ino) != identity):
            raise NormalizeRefused(f'private driver staging member identity changed: {name}')


def _read_regular(path: Path, limit: int) -> bytes:
    try:
        descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    except OSError as error:
        raise NormalizeRefused(f'{path.name} is not an admitted regular file') from error
    try:
        before = os.fstat(descriptor)
        if not stat.S_ISREG(before.st_mode) or before.st_size <= 0 or before.st_size > limit:
            raise NormalizeRefused(f'{path.name} exceeds its regular-file bound')
        chunks: list[bytes] = []
        total = 0
        while True:
            chunk = os.read(descriptor, min(1024 * 1024, limit + 1 - total))
            if not chunk:
                break
            chunks.append(chunk)
            total += len(chunk)
            if total > limit:
                raise NormalizeRefused(f'{path.name} exceeds its regular-file bound')
        after = os.fstat(descriptor)
    finally:
        os.close(descriptor)
    first = (before.st_dev, before.st_ino, before.st_mode, before.st_size,
             before.st_mtime_ns, before.st_ctime_ns)
    last = (after.st_dev, after.st_ino, after.st_mode, after.st_size,
            after.st_mtime_ns, after.st_ctime_ns)
    data = b''.join(chunks)
    if first != last or len(data) != before.st_size:
        raise NormalizeRefused(f'{path.name} changed while it was read')
    return data


def _elf_metadata(data: bytes, *, require_soname: bool = True) -> tuple[str | None, tuple[str, ...]]:
    if len(data) < ELF_HEADER.size:
        raise NormalizeRefused('captured object has a truncated ELF header')
    header = ELF_HEADER.unpack_from(data)
    if header[0][:7] != b'\x7fELF\x02\x01\x01' or header[1:4] != (ET_DYN, EM_X86_64, 1):
        raise NormalizeRefused('captured object is not ELF64 x86_64')
    phoff, phentsize, phnum = header[5], header[9], header[10]
    if phentsize != PROGRAM_HEADER.size or not phnum or phnum > 1024:
        raise NormalizeRefused('captured object has unsupported ELF program headers')
    phsize = phentsize * phnum
    if phoff < ELF_HEADER.size or phoff + phsize > len(data):
        raise NormalizeRefused('captured object ELF headers escape the file')
    loads: list[tuple[int, int, int]] = []
    dynamic: tuple[int, int] | None = None
    for index in range(phnum):
        item = PROGRAM_HEADER.unpack_from(data, phoff + index * phentsize)
        kind, file_offset, virtual, file_size, memory_size = item[0], item[2], item[3], item[5], item[6]
        if file_offset + file_size > len(data) or file_size > memory_size:
            raise NormalizeRefused('captured object has an invalid ELF segment')
        if kind == PT_LOAD:
            loads.append((virtual, file_offset, file_size))
        elif kind == PT_DYNAMIC:
            if dynamic is not None:
                raise NormalizeRefused('captured object has multiple PT_DYNAMIC segments')
            dynamic = (file_offset, file_size)
    if dynamic is None:
        raise NormalizeRefused('captured object has no PT_DYNAMIC segment')
    dynamic_offset, dynamic_size = dynamic
    if dynamic_size == 0 or dynamic_size % DYNAMIC.size or dynamic_offset + dynamic_size > len(data):
        raise NormalizeRefused('captured object has an invalid dynamic segment')
    values: list[tuple[int, int]] = []
    for offset in range(dynamic_offset, dynamic_offset + dynamic_size, DYNAMIC.size):
        tag, value = DYNAMIC.unpack_from(data, offset)
        if tag == DT_NULL:
            break
        values.append((tag, value))
    strtab = next((value for tag, value in values if tag == DT_STRTAB), None)
    strsize = next((value for tag, value in values if tag == DT_STRSZ), None)
    if type(strtab) is not int or type(strsize) is not int or not 0 < strsize <= MAX_FILE_BYTES:
        raise NormalizeRefused('captured object dynamic string table is invalid')
    offsets = [file_offset + strtab - virtual for virtual, file_offset, file_size in loads
               if virtual <= strtab and strtab + strsize <= virtual + file_size]
    if len(offsets) != 1 or offsets[0] + strsize > len(data):
        raise NormalizeRefused('captured object dynamic strings have no unique file mapping')
    strings = data[offsets[0]:offsets[0] + strsize]

    def get_string(offset: int) -> str:
        if type(offset) is not int or offset < 0 or offset >= len(strings):
            raise NormalizeRefused('captured object dynamic string offset is invalid')
        end = strings.find(b'\0', offset)
        if end < 0:
            raise NormalizeRefused('captured object dynamic string is unterminated')
        try:
            value = strings[offset:end].decode('ascii')
        except UnicodeDecodeError as error:
            raise NormalizeRefused('captured object dynamic string is not ASCII') from error
        if not value or '/' in value or '\\' in value:
            raise NormalizeRefused('captured object SONAME is not a leaf')
        return value

    soname_offset = next((value for tag, value in values if tag == DT_SONAME), None)
    if soname_offset is None and not require_soname:
        soname = None
    elif type(soname_offset) is not int:
        raise NormalizeRefused('captured object has no DT_SONAME')
    else:
        soname = get_string(soname_offset)
    needed = tuple(get_string(value) for tag, value in values if tag == DT_NEEDED)
    return soname, needed


def _realization_set() -> dict[str, dict[str, object]]:
    try:
        raw = json.loads(os.environ.get('RYEOS_EXTERNAL_REALIZATIONS', 'null'))
    except json.JSONDecodeError as error:
        raise NormalizeRefused('sealed realization selection is invalid JSON') from error
    keys = {'id', 'kind', 'mode', 'manifest_hash', 'entry_count', 'total_bytes', 'mount_root', 'mount'}
    if (not isinstance(raw, list) or len(raw) != 2
            or any(not isinstance(item, dict) or set(item) != keys for item in raw)):
        raise NormalizeRefused('normalizer requires exactly its pinned runtime and GNU product')
    selected = {item['id']: item for item in raw}
    if set(selected) != {'producer-python', 'gnu_runtime'}:
        raise NormalizeRefused('normalizer selected an unexpected product set')
    producer, gnu = selected['producer-python'], selected['gnu_runtime']
    if (producer['kind'] != 'tree' or producer['mode'] != 'pinned'
            or producer['mount_root'] != 'execution_runtime' or producer['mount'] != 'producer-python'
            or producer['manifest_hash'] != PRODUCER_PYTHON_HASH):
        raise NormalizeRefused('normalizer pinned Python realization changed')
    if (gnu['kind'] != 'tree' or gnu['mode'] != 'pinned'
            or gnu['mount_root'] != 'execution_runtime' or gnu['mount'] != 'python-gnu-private'
            or not isinstance(gnu['manifest_hash'], str) or _HASH.fullmatch(gnu['manifest_hash']) is None
            or type(gnu['entry_count']) is not int or not 1 <= gnu['entry_count'] <= MAX_ENTRIES
            or type(gnu['total_bytes']) is not int or not 1 <= gnu['total_bytes'] <= MAX_TOTAL_BYTES):
        raise NormalizeRefused('selected GNU product identity or bounds are incomplete')
    return selected


def _gnu_sonames(root: Path, realization: Mapping[str, object]) -> set[str]:
    root = root.resolve(strict=True)
    library_root = (root / 'python' / 'lib').resolve(strict=True)
    if root not in library_root.parents or not library_root.is_dir():
        raise NormalizeRefused('selected GNU library root escaped its product')
    names: set[str] = set()
    visited: set[Path] = set()
    entries = total_bytes = 0
    for member in library_root.rglob('*'):
        entries += 1
        relative = member.relative_to(library_root)
        if entries > int(realization['entry_count']) or len(relative.parts) > MAX_DEPTH:
            raise NormalizeRefused('selected GNU library tree exceeds entry/depth bounds')
        if member.is_symlink():
            resolved = member.resolve(strict=True)
            if library_root not in resolved.parents or resolved.is_dir():
                raise NormalizeRefused('selected GNU library symlink escapes or names a directory')
        elif member.is_dir():
            continue
        elif not member.is_file():
            raise NormalizeRefused('selected GNU library tree has an unsupported member')
        resolved = member.resolve(strict=True)
        if library_root not in resolved.parents or resolved in visited:
            continue
        visited.add(resolved)
        if '.so' not in resolved.name:
            continue
        data = _read_regular(resolved, MAX_FILE_BYTES)
        total_bytes += len(data)
        if len(data) > MAX_FILE_BYTES or total_bytes > int(realization['total_bytes']):
            raise NormalizeRefused('selected GNU library bytes exceed admitted bounds')
        soname, _ = _elf_metadata(data, require_soname=False)
        if soname is not None:
            names.add(soname)
    return names


def _normalize_capture_inner(
    *,
    capture_fd: int,
    output_parent_fd: int,
    output_name: str,
    realizations: Mapping[str, Mapping[str, object]],
    gnu_root: Path = Path('/ryeos/realizations/python-gnu-private'),
) -> dict[str, object]:
    try:
        record_bytes = _read_regular_at(capture_fd, 'capture.json', 1_048_576)
        capture = json.loads(record_bytes)
    except (OSError, json.JSONDecodeError) as error:
        raise NormalizeRefused('guest capture record is absent or invalid') from error
    if (not isinstance(capture, dict) or capture.get('schema') != CAPTURE_SCHEMA
            or capture.get('coverage') != 'initial_load_only_unqualified'
            or capture.get('requested_sonames') != list(REQUESTED_SONAMES)
            or capture.get('loaded_entry_sonames') != list(REQUESTED_SONAMES)):
        raise NormalizeRefused('guest capture record has an unexpected scope or SONAME set')
    files = capture.get('files')
    if not isinstance(files, list) or not 2 <= len(files) <= MAX_ENTRIES:
        raise NormalizeRefused('guest capture has an invalid file inventory')
    gnu = realizations['gnu_runtime']
    gnu_sonames = _gnu_sonames(gnu_root, gnu)
    staged: dict[str, tuple[bytes, str]] = {}
    seen_file_paths: set[str] = set()
    total_input = 0
    for item in files:
        if not isinstance(item, dict) or set(item) != {
            'soname', 'needed', 'relative_path', 'sha256', 'mapped_path',
            'symlink_edges', 'mapped_identity', 'newly_mapped_by_initial_load',
            'loader_name_observation',
        }:
            raise NormalizeRefused('guest capture file record has an unexpected shape')
        soname = item['soname']
        if (not isinstance(soname, str) or '.so' not in soname
                or soname in ('.', '..') or len(soname) > 255
                or '/' in soname or '\\' in soname or not isinstance(item['needed'], list)
                or any(not isinstance(name, str) or '/' in name for name in item['needed'])):
            raise NormalizeRefused('guest capture SONAME metadata is not canonical')
        if item['newly_mapped_by_initial_load'] is not True:
            if item['relative_path'] is not None:
                raise NormalizeRefused('pre-existing guest dependency unexpectedly contains bytes')
            continue
        relative = item['relative_path']
        if (not isinstance(relative, str) or not relative.startswith('files/')
                or Path(relative).is_absolute() or '..' in Path(relative).parts
                or relative in seen_file_paths or not isinstance(item['sha256'], str)
                or _HASH.fullmatch(item['sha256']) is None):
            raise NormalizeRefused('guest capture member path or hash is invalid')
        seen_file_paths.add(relative)
        data = _read_capture_member(capture_fd, relative, MAX_FILE_BYTES)
        total_input += len(data)
        if total_input > MAX_TOTAL_BYTES or hashlib.sha256(data).hexdigest() != item['sha256']:
            raise NormalizeRefused('guest capture member changed or exceeds its total-byte bound')
        observed_soname, observed_needed = _elf_metadata(data)
        if observed_soname != soname or list(observed_needed) != item['needed']:
            raise NormalizeRefused('guest capture SONAME/NEEDED record differs from retained ELF bytes')
        if soname in gnu_sonames:
            # GNU owns providers present in the exact selected GNU product.
            # Never copy ambient guest libc/libgcc/etc into the driver product.
            continue
        previous = staged.get(soname)
        if previous is not None and hashlib.sha256(previous[0]).hexdigest() != item['sha256']:
            raise NormalizeRefused(f'guest capture contains ambiguous {soname} providers')
        staged[soname] = (data, item['sha256'])
    for soname in REQUESTED_SONAMES:
        if soname not in staged:
            raise NormalizeRefused(f'initial driver capture did not retain required {soname}')

    stage_name, stage_fd, stage_identity = _new_stage(output_parent_fd)
    library_fd = metadata_fd = -1
    library_identity: tuple[int, int] | None = None
    metadata_identity: tuple[int, int] | None = None
    library_written: dict[str, tuple[int, int]] = {}
    metadata_written: dict[str, tuple[int, int]] = {}
    try:
        os.mkdir('lib', mode=0o700, dir_fd=stage_fd)
        os.mkdir('.ryeos', mode=0o700, dir_fd=stage_fd)
        library_fd = os.open('lib', _DIR_FLAGS, dir_fd=stage_fd)
        metadata_fd = os.open('.ryeos', _DIR_FLAGS, dir_fd=stage_fd)
        library_info, metadata_info = os.fstat(library_fd), os.fstat(metadata_fd)
        library_identity = (library_info.st_dev, library_info.st_ino)
        metadata_identity = (metadata_info.st_dev, metadata_info.st_ino)
        members = []
        for soname in sorted(staged):
            data, digest = staged[soname]
            if len(data) > MAX_FILE_BYTES:
                raise NormalizeRefused(f'{soname} exceeds the product file bound')
            library_written[soname] = _write_at(library_fd, soname, data)
            members.append({'soname': soname, 'relative_path': f'lib/{soname}', 'sha256': digest})
        metadata_written['modal-driver-capture.json'] = _write_at(
            metadata_fd, 'modal-driver-capture.json', record_bytes,
        )
        binding = {
            'schema': PRODUCT_RECORD_SCHEMA,
            'coverage': 'static_input_binding_only',
            'capture_record_sha256': hashlib.sha256(record_bytes).hexdigest(),
            'gnu_manifest_hash': gnu['manifest_hash'],
            'gnu_abi_identity': None,
            'driver_members': members,
            'claims': [],
        }
        metadata_written['normalization.json'] = _write_at(
            metadata_fd, 'normalization.json',
            (json.dumps(binding, sort_keys=True, separators=(',', ':')) + '\n').encode(),
        )
        _verify_stage(output_parent_fd, stage_name, stage_fd, stage_identity)
        _verify_child(stage_fd, 'lib', library_fd, library_identity)
        _verify_child(stage_fd, '.ryeos', metadata_fd, metadata_identity)
        if set(os.listdir(stage_fd)) != {'lib', '.ryeos'}:
            raise NormalizeRefused('private driver staging child inventory changed')
        _verify_inventory(library_fd, library_written)
        _verify_inventory(metadata_fd, metadata_written)
        _rename_noreplace(output_parent_fd, stage_name, output_name)
        _verify_stage(output_parent_fd, output_name, stage_fd, stage_identity)
        _verify_child(stage_fd, 'lib', library_fd, library_identity)
        _verify_child(stage_fd, '.ryeos', metadata_fd, metadata_identity)
        if set(os.listdir(stage_fd)) != {'lib', '.ryeos'}:
            raise NormalizeRefused('published driver child inventory changed')
        _verify_inventory(library_fd, library_written)
        _verify_inventory(metadata_fd, metadata_written)
    except BaseException:
        # Leave the bounded private stage quarantined for its workspace owner.
        # Name-based deletion is unsafe without writer exclusion.
        raise
    finally:
        if library_fd >= 0:
            os.close(library_fd)
        if metadata_fd >= 0:
            os.close(metadata_fd)
        os.close(stage_fd)
    return binding


def normalize_capture(
    capture_root: Path,
    output_root: Path,
    *,
    realizations: Mapping[str, Mapping[str, object]],
    gnu_root: Path = Path('/ryeos/realizations/python-gnu-private'),
    workspace_root: Path | None = None,
) -> dict[str, object]:
    workspace_root = Path.cwd() if workspace_root is None else Path(workspace_root)
    workspace_fd = _open_workspace_root(workspace_root)
    capture_fd = output_parent_fd = -1
    try:
        capture_fd = _open_workspace_dir(workspace_fd, Path(capture_root), workspace_root)
        output_parent_fd, output_name = _open_output_parent(
            workspace_fd, Path(output_root), workspace_root,
        )
        return _normalize_capture_inner(
            capture_fd=capture_fd,
            output_parent_fd=output_parent_fd, output_name=output_name,
            realizations=realizations, gnu_root=gnu_root,
        )
    finally:
        if output_parent_fd >= 0:
            os.close(output_parent_fd)
        if capture_fd >= 0:
            os.close(capture_fd)
        os.close(workspace_fd)


def main() -> int:
    try:
        request = json.load(sys.stdin)
    except (UnicodeDecodeError, json.JSONDecodeError) as error:
        raise NormalizeRefused('normalizer request is not valid JSON') from error
    if (not isinstance(request, dict) or set(request) != {'schema', 'capture_root', 'output_root'}
            or request['schema'] != REQUEST_SCHEMA
            or request['capture_root'] != 'inputs/modal-driver-capture'
            or request['output_root'] != 'products/modal-guest-user-driver'):
        raise NormalizeRefused('normalizer request differs from its signed workspace paths')
    selected = _realization_set()
    binding = normalize_capture(
        Path.cwd() / request['capture_root'], Path.cwd() / request['output_root'],
        realizations=selected,
    )
    print(json.dumps(binding, sort_keys=True, separators=(',', ':')))
    return 0


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except NormalizeRefused as error:
        print(f'normalizer refused: {error}', file=sys.stderr)
        raise SystemExit(2)
