"""Offline structural ELF closure checks; no native object is loaded."""
from pathlib import Path
import struct
import sys
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parents[2] / '.ai/workers/local-inference/lib/local-tinygrad-gnu'
sys.dont_write_bytecode = True
sys.path.insert(0, str(SOURCE))
import bootstrap_support
import driver_closure


def _shared_object(path: Path, soname: str, needed=(), runpath: str | None = None) -> None:
    strings = bytearray(b'\0')

    def intern(value: str) -> int:
        offset = len(strings)
        strings.extend(value.encode('ascii') + b'\0')
        return offset

    dynamic = [(driver_closure._DT_NEEDED, intern(item)) for item in needed]
    dynamic.append((driver_closure._DT_SONAME, intern(soname)))
    if runpath is not None:
        dynamic.append((driver_closure._DT_RUNPATH, intern(runpath)))
    dynamic.append((driver_closure._DT_STRTAB, 0))
    dynamic.append((driver_closure._DT_STRSZ, len(strings)))
    dynamic.append((driver_closure._DT_NULL, 0))
    dynamic_offset = 64 + 2 * driver_closure._PROGRAM_HEADER.size
    dynamic_bytes = b''.join(driver_closure._DYNAMIC.pack(tag, value) for tag, value in dynamic)
    string_offset = dynamic_offset + len(dynamic_bytes)
    dynamic[-3] = (driver_closure._DT_STRTAB, string_offset)
    dynamic[-2] = (driver_closure._DT_STRSZ, len(strings))
    dynamic_bytes = b''.join(driver_closure._DYNAMIC.pack(tag, value) for tag, value in dynamic)
    string_offset = dynamic_offset + len(dynamic_bytes)
    dynamic[-3] = (driver_closure._DT_STRTAB, string_offset)
    dynamic_bytes = b''.join(driver_closure._DYNAMIC.pack(tag, value) for tag, value in dynamic)
    end = string_offset + len(strings)
    ident = b'\x7fELF\x02\x01\x01\0' + b'\0' * 8
    header = driver_closure._ELF_HEADER.pack(
        ident, driver_closure._ET_DYN, driver_closure._EM_X86_64, 1,
        0, 64, 0, 0, 64, driver_closure._PROGRAM_HEADER.size, 2, 0, 0, 0,
    )
    load = driver_closure._PROGRAM_HEADER.pack(1, 5, 0, 0, 0, end, end, 4096)
    dynamic_header = driver_closure._PROGRAM_HEADER.pack(
        driver_closure._PT_DYNAMIC, 4, dynamic_offset, dynamic_offset,
        dynamic_offset, len(dynamic_bytes), len(dynamic_bytes), 8,
    )
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(header + load + dynamic_header + dynamic_bytes + strings)


class DriverClosureTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.gnu = self.root / 'gnu/lib'
        self.driver = self.root / 'driver/lib'
        self.gnu.mkdir(parents=True)
        self.driver.mkdir(parents=True)
        _shared_object(self.gnu / 'libc.so.6', 'libc.so.6')
        _shared_object(
            self.driver / 'libcuda.so.1', 'libcuda.so.1',
            ('libhelper.so.1', 'libc.so.6'), '$ORIGIN',
        )
        _shared_object(
            self.driver / 'libhelper.so.1', 'libhelper.so.1',
            ('libc.so.6',), '$ORIGIN',
        )
        self.gnu_bounds = driver_closure.TreeBounds(8, 4, 1_000_000, 2_000_000)
        self.driver_bounds = driver_closure.TreeBounds(8, 4, 1_000_000, 2_000_000)

    def inspect(self):
        return driver_closure.inspect_cuda_driver_closure(
            self.gnu, self.driver,
            gnu_bounds=self.gnu_bounds,
            driver_bounds=self.driver_bounds,
        )

    def test_transitive_closure_is_unique_rooted_and_loader_reachable(self):
        closure = self.inspect()
        self.assertEqual(
            [path.name for path in closure.load_order],
            ['libc.so.6', 'libhelper.so.1', 'libcuda.so.1'],
        )
        resolver = bootstrap_support.exact_library_resolver(
            self.gnu / 'libc.so.6', self.root / 'driver',
            selected_libraries=closure.providers,
        )
        self.assertEqual(resolver('libhelper.so.1', ['/ambient']), str(self.driver / 'libhelper.so.1'))
        self.assertIsNone(resolver('nvrtc', ['/ambient']))
        with self.assertRaisesRegex(RuntimeError, 'unselected library'):
            resolver('unselected.so', ['/ambient'])
        poisoned_map = bootstrap_support.exact_library_resolver(
            self.gnu / 'libc.so.6', self.root / 'driver',
            selected_libraries={'nvrtc': self.driver / 'libhelper.so.1'},
        )
        self.assertIsNone(poisoned_map('nvrtc', ['/ambient']))

    def test_missing_soname_and_unreachable_provider_refuse(self):
        (self.driver / 'libhelper.so.1').unlink()
        with self.assertRaisesRegex(driver_closure.DriverClosureError, 'missing selected'):
            self.inspect()
        _shared_object(self.driver / 'libhelper.so.1', 'libhelper.so.1', ('libcuda.so.1',))
        with self.assertRaisesRegex(driver_closure.DriverClosureError, 'not reachable'):
            self.inspect()

    def test_ambiguous_provider_and_escaping_symlink_refuse(self):
        _shared_object(self.driver / 'duplicate.so', 'libhelper.so.1')
        with self.assertRaisesRegex(driver_closure.DriverClosureError, 'ambiguous'):
            self.inspect()
        (self.driver / 'duplicate.so').unlink()
        outside = self.root / 'outside.so'
        _shared_object(outside, 'unused.so')
        (self.driver / 'escape.so').symlink_to(outside)
        with self.assertRaisesRegex(driver_closure.DriverClosureError, 'escaped'):
            self.inspect()

    def test_admitted_bounds_are_required_and_enforced(self):
        with self.assertRaisesRegex(driver_closure.DriverClosureError, 'exact admitted tree bounds'):
            driver_closure.TreeBounds.from_contract({'maximum_entries': 8})
        with self.assertRaisesRegex(driver_closure.DriverClosureError, 'entry/depth bounds'):
            driver_closure.inspect_cuda_driver_closure(
                self.gnu, self.driver,
                gnu_bounds=self.gnu_bounds,
                driver_bounds=driver_closure.TreeBounds(1, 4, 1_000_000, 2_000_000),
            )

    def test_escaping_search_paths_and_selected_library_maps_refuse(self):
        _shared_object(
            self.driver / 'libcuda.so.1', 'libcuda.so.1',
            ('libhelper.so.1', 'libc.so.6'), '/tmp',
        )
        with self.assertRaisesRegex(driver_closure.DriverClosureError, 'escapes'):
            self.inspect()
        outside = self.root / 'outside.so'
        _shared_object(outside, 'outside.so')
        with self.assertRaisesRegex(ValueError, 'escaped'):
            bootstrap_support.exact_library_resolver(
                self.gnu / 'libc.so.6', self.root / 'driver',
                selected_libraries={'ambient.so': self.root / 'outside.so'},
            )


if __name__ == '__main__':
    unittest.main()
