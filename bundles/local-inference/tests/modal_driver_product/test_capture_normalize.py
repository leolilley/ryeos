"""Offline projected-driver capture and normalization checks."""
from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock


ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / 'tests/gnu_cuda_runtime'
sys.path.insert(0, str(FIXTURES))
from test_driver_closure import _shared_object


def _load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


CAPTURE = _load('modal_driver_projection_capture', ROOT / 'authoring/modal_driver_projection_capture.py')
NORMALIZE = _load(
    'modal_driver_normalize',
    ROOT / '.ai/tools/local-inference/modal-driver-normalize/normalize.py',
)


class CaptureNormalizeTests(unittest.TestCase):
    def test_fixed_load_set_is_captured_and_normalized_against_exact_gnu_tree(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            guest = root / 'guest'
            guest.mkdir()
            _shared_object(guest / 'libcuda.so.1', 'libcuda.so.1', ('libc.so.6',))
            _shared_object(guest / 'libnvidia-ml.so.1', 'libnvidia-ml.so.1', ('libc.so.6',))
            _shared_object(guest / 'libc.so.6', 'libc.so.6')
            _shared_object(guest / 'ld-linux-x86-64.so.2', 'ld-linux-x86-64.so.2')

            def identity(path: Path):
                info = path.stat()
                key = (os.major(info.st_dev), os.minor(info.st_dev), info.st_ino)
                return key, {
                    'device_major': key[0], 'device_minor': key[1], 'inode': key[2],
                    'path': str(path), 'ranges': ['1000-2000'],
                }

            entries = [identity(guest / name) for name in (
                'libcuda.so.1', 'libnvidia-ml.so.1', 'libc.so.6', 'ld-linux-x86-64.so.2',
            )]
            snapshots = iter(({}, dict(entries)))
            requested: list[str] = []
            capture_root = root / 'inputs/modal-driver-capture'
            record = CAPTURE.capture_projection(
                capture_root, load_library=lambda name: requested.append(name) or object(),
                snapshot=lambda: next(snapshots), guest_image='fixture-image', workspace_root=root,
            )
            self.assertEqual(requested, ['libcuda.so.1', 'libnvidia-ml.so.1'])
            self.assertEqual(record['loaded_entry_sonames'], requested)
            self.assertEqual({item['soname'] for item in record['files']}, {
                'libcuda.so.1', 'libnvidia-ml.so.1', 'libc.so.6',
            })

            gnu = root / 'gnu'
            gnu_lib = gnu / 'python/lib'
            gnu_lib.mkdir(parents=True)
            _shared_object(gnu_lib / 'libc.so.6', 'libc.so.6')
            output = root / 'products/modal-guest-user-driver'
            realization = {
                'id': 'gnu_runtime', 'kind': 'tree', 'mode': 'pinned',
                'manifest_hash': 'a' * 64, 'entry_count': 32, 'total_bytes': 1_000_000,
                'mount_root': 'execution_runtime', 'mount': 'python-gnu-private',
            }
            binding = NORMALIZE.normalize_capture(
                capture_root, output,
                realizations={'gnu_runtime': realization}, gnu_root=gnu, workspace_root=root,
            )
            self.assertEqual([member['soname'] for member in binding['driver_members']], [
                'libcuda.so.1', 'libnvidia-ml.so.1',
            ])
            self.assertTrue((output / 'lib/libcuda.so.1').is_file())
            self.assertTrue((output / 'lib/libnvidia-ml.so.1').is_file())
            self.assertFalse((output / 'lib/libc.so.6').exists())
            self.assertEqual(binding['claims'], [])

    def test_existing_capture_output_is_not_overwritten(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            output = root / 'capture'
            output.mkdir()
            loads: list[str] = []
            with self.assertRaisesRegex(CAPTURE.CaptureRefused, 'output root already exists'):
                CAPTURE.capture_projection(
                    output, load_library=lambda soname: loads.append(soname), snapshot=lambda: {},
                    workspace_root=root,
                )
            self.assertEqual(loads, [])
            with mock.patch.object(CAPTURE.ctypes, 'CDLL', side_effect=AssertionError('loader called')):
                with self.assertRaisesRegex(CAPTURE.CaptureRefused, 'output root already exists'):
                    CAPTURE.capture_projection(output, snapshot=lambda: {}, workspace_root=root)

    def test_capture_refuses_parent_symlink_before_any_load(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            outside = root / 'outside'
            outside.mkdir()
            (root / 'inputs').symlink_to(outside, target_is_directory=True)
            loads: list[str] = []
            with self.assertRaisesRegex(CAPTURE.CaptureRefused, 'no-symlink directory'):
                CAPTURE.capture_projection(
                    root / 'inputs/modal-driver-capture',
                    load_library=lambda soname: loads.append(soname), snapshot=lambda: {},
                    workspace_root=root,
                )
            self.assertEqual(loads, [])

    def test_normalizer_refuses_redirected_capture_root(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            outside = root / 'outside'
            outside.mkdir()
            (outside / 'capture').mkdir()
            (root / 'inputs').symlink_to(outside, target_is_directory=True)
            with self.assertRaisesRegex(
                NORMALIZE.NormalizeRefused, 'workspace input path is absent or contains a symlink',
            ):
                NORMALIZE.normalize_capture(
                    root / 'inputs/capture', root / 'products/output', realizations={},
                    gnu_root=root, workspace_root=root,
                )

    def test_normalizer_refuses_redirected_output_parent(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'inputs/capture').mkdir(parents=True)
            outside = root / 'outside'
            outside.mkdir()
            (root / 'products').symlink_to(outside, target_is_directory=True)
            with self.assertRaisesRegex(NORMALIZE.NormalizeRefused, 'no-symlink directory'):
                NORMALIZE.normalize_capture(
                    root / 'inputs/capture', root / 'products/output', realizations={},
                    gnu_root=root, workspace_root=root,
                )

    def test_create_only_capture_rename_preserves_racing_destination(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / 'stage'
            target = root / 'capture'
            source.mkdir()
            target.mkdir()
            (target / 'owner.txt').write_text('preserve')
            descriptor = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
            try:
                with self.assertRaisesRegex(CAPTURE.CaptureRefused, 'already exists'):
                    CAPTURE._rename_noreplace(descriptor, 'stage', 'capture')
            finally:
                os.close(descriptor)
            self.assertEqual((target / 'owner.txt').read_text(), 'preserve')

    def test_capture_stage_descriptor_writes_leave_replaced_stage_quarantined(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            outside = root / 'outside'
            outside.mkdir()
            parent_fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
            try:
                name, stage_fd, identity = CAPTURE._new_stage(parent_fd)
                os.mkdir('files', dir_fd=stage_fd)
                child_fd = os.open('files', os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                                   dir_fd=stage_fd)
                child_info = os.fstat(child_fd)
                child_identity = (child_info.st_dev, child_info.st_ino)
                CAPTURE._write_at(child_fd, 'retained.bin', b'original')
                os.rename(name, 'moved-stage', src_dir_fd=parent_fd, dst_dir_fd=parent_fd)
                os.symlink(outside, name, target_is_directory=True, dir_fd=parent_fd)
                with self.assertRaisesRegex(CAPTURE.CaptureRefused, 'identity changed'):
                    CAPTURE._verify_stage(parent_fd, name, stage_fd, identity)
                CAPTURE._write_at(child_fd, 'after-swap.bin', b'fd-bound')
                self.assertFalse((outside / 'after-swap.bin').exists())
                self.assertTrue(os.path.islink(root / name))
                self.assertEqual((root / 'moved-stage/files/retained.bin').read_bytes(), b'original')
                self.assertEqual((root / 'moved-stage/files/after-swap.bin').read_bytes(), b'fd-bound')
                os.close(child_fd)
                os.close(stage_fd)
            finally:
                os.close(parent_fd)

    def test_normalizer_stage_descriptor_writes_leave_replaced_stage_quarantined(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            outside = root / 'outside'
            outside.mkdir()
            parent_fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
            try:
                name, stage_fd, identity = NORMALIZE._new_stage(parent_fd)
                os.mkdir('lib', dir_fd=stage_fd)
                child_fd = os.open('lib', os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                                   dir_fd=stage_fd)
                child_info = os.fstat(child_fd)
                child_identity = (child_info.st_dev, child_info.st_ino)
                NORMALIZE._write_at(child_fd, 'retained.bin', b'original')
                os.rename(name, 'moved-stage', src_dir_fd=parent_fd, dst_dir_fd=parent_fd)
                os.symlink(outside, name, target_is_directory=True, dir_fd=parent_fd)
                with self.assertRaisesRegex(NORMALIZE.NormalizeRefused, 'identity changed'):
                    NORMALIZE._verify_stage(parent_fd, name, stage_fd, identity)
                NORMALIZE._write_at(child_fd, 'after-swap.bin', b'fd-bound')
                self.assertFalse((outside / 'after-swap.bin').exists())
                self.assertTrue(os.path.islink(root / name))
                self.assertEqual((root / 'moved-stage/lib/retained.bin').read_bytes(), b'original')
                self.assertEqual((root / 'moved-stage/lib/after-swap.bin').read_bytes(), b'fd-bound')
                os.close(child_fd)
                os.close(stage_fd)
            finally:
                os.close(parent_fd)

    def test_capture_child_directory_swap_is_not_followed_or_deleted(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            outside = root / 'outside'
            outside.mkdir()
            parent_fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
            try:
                name, stage_fd, identity = CAPTURE._new_stage(parent_fd)
                os.mkdir('files', dir_fd=stage_fd)
                child_fd = os.open('files', os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                                   dir_fd=stage_fd)
                child_info = os.fstat(child_fd)
                child_identity = (child_info.st_dev, child_info.st_ino)
                CAPTURE._write_at(child_fd, 'retained.bin', b'owned')
                os.rename('files', 'moved-files', src_dir_fd=stage_fd, dst_dir_fd=stage_fd)
                os.symlink(outside, 'files', target_is_directory=True, dir_fd=stage_fd)
                with self.assertRaisesRegex(CAPTURE.CaptureRefused, 'child directory identity changed'):
                    CAPTURE._verify_child(stage_fd, 'files', child_fd, child_identity)
                CAPTURE._write_at(child_fd, 'after-swap.bin', b'fd-bound')
                self.assertFalse((outside / 'after-swap.bin').exists())
                self.assertEqual(set(os.listdir(child_fd)), {'retained.bin', 'after-swap.bin'})
                self.assertTrue((Path(f'/proc/self/fd/{stage_fd}') / 'files').is_symlink())
                os.close(child_fd)
                os.close(stage_fd)
            finally:
                os.close(parent_fd)

    def test_normalizer_child_directory_swap_is_not_followed_or_deleted(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            outside = root / 'outside'
            outside.mkdir()
            parent_fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
            try:
                name, stage_fd, identity = NORMALIZE._new_stage(parent_fd)
                os.mkdir('lib', dir_fd=stage_fd)
                child_fd = os.open('lib', os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW,
                                   dir_fd=stage_fd)
                child_info = os.fstat(child_fd)
                child_identity = (child_info.st_dev, child_info.st_ino)
                NORMALIZE._write_at(child_fd, 'retained.bin', b'owned')
                os.rename('lib', 'moved-lib', src_dir_fd=stage_fd, dst_dir_fd=stage_fd)
                os.symlink(outside, 'lib', target_is_directory=True, dir_fd=stage_fd)
                with self.assertRaisesRegex(NORMALIZE.NormalizeRefused, 'child directory identity changed'):
                    NORMALIZE._verify_child(stage_fd, 'lib', child_fd, child_identity)
                NORMALIZE._write_at(child_fd, 'after-swap.bin', b'fd-bound')
                self.assertFalse((outside / 'after-swap.bin').exists())
                self.assertEqual(set(os.listdir(child_fd)), {'retained.bin', 'after-swap.bin'})
                self.assertTrue((Path(f'/proc/self/fd/{stage_fd}') / 'lib').is_symlink())
                os.close(child_fd)
                os.close(stage_fd)
            finally:
                os.close(parent_fd)

    def test_normalizer_gnu_index_accepts_python_extension_without_soname(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            library_root = root / 'python/lib'
            library_root.mkdir(parents=True)
            _shared_object(library_root / 'libc.so.6', 'libc.so.6')
            extension = library_root / 'example_extension.so'
            _shared_object(extension, 'example_extension.so')
            data = bytearray(extension.read_bytes())
            phoff = int.from_bytes(data[32:40], 'little')
            phentsize = int.from_bytes(data[54:56], 'little')
            phnum = int.from_bytes(data[56:58], 'little')
            for index in range(phnum):
                header = phoff + index * phentsize
                if int.from_bytes(data[header:header + 4], 'little') != 2:
                    continue
                dynamic_offset = int.from_bytes(data[header + 8:header + 16], 'little')
                dynamic_size = int.from_bytes(data[header + 32:header + 40], 'little')
                for offset in range(dynamic_offset, dynamic_offset + dynamic_size, 16):
                    if int.from_bytes(data[offset:offset + 8], 'little', signed=True) == 14:
                        data[offset:offset + 8] = (2).to_bytes(8, 'little', signed=True)
            extension.write_bytes(data)
            realization = {'entry_count': 16, 'total_bytes': 1_000_000}
            self.assertEqual(NORMALIZE._gnu_sonames(root, realization), {'libc.so.6'})


if __name__ == '__main__':
    unittest.main()
