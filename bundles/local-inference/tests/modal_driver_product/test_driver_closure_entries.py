"""Offline checks for the shared CUDA+NVML static closure parser."""
from pathlib import Path
import struct
import sys
import tempfile
import unittest

SOURCE = Path(__file__).resolve().parents[2] / '.ai/workers/local-inference/lib/local-tinygrad-gnu'
sys.dont_write_bytecode = True
sys.path.insert(0, str(SOURCE))
import driver_closure


def _shared_object(path: Path, soname: str, needed=()) -> None:
    strings = bytearray(b'\0')

    def intern(value: str) -> int:
        offset = len(strings)
        strings.extend(value.encode('ascii') + b'\0')
        return offset

    dynamic = [(driver_closure._DT_NEEDED, intern(item)) for item in needed]
    dynamic.append((driver_closure._DT_SONAME, intern(soname)))
    dynamic.extend(((driver_closure._DT_STRTAB, 0),
                    (driver_closure._DT_STRSZ, len(strings)),
                    (driver_closure._DT_NULL, 0)))
    phnum = 2
    dynamic_offset = 64 + phnum * driver_closure._PROGRAM_HEADER.size
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
        0, 64, 0, 0, 64, driver_closure._PROGRAM_HEADER.size, phnum, 0, 0, 0,
    )
    load = driver_closure._PROGRAM_HEADER.pack(1, 5, 0, 0, 0, end, end, 4096)
    dynamic_header = driver_closure._PROGRAM_HEADER.pack(
        2, 4, dynamic_offset, dynamic_offset, dynamic_offset,
        len(dynamic_bytes), len(dynamic_bytes), 8,
    )
    path.write_bytes(header + load + dynamic_header + dynamic_bytes + strings)


class DriverClosureEntryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.base = Path(self.temp.name)
        self.gnu = self.base / 'gnu' / 'python' / 'lib'
        self.driver = self.base / 'driver' / 'lib'
        self.gnu.mkdir(parents=True)
        self.driver.mkdir(parents=True)
        _shared_object(self.gnu / 'libc.so.6', 'libc.so.6')
        _shared_object(self.driver / 'libcuda.so.1', 'libcuda.so.1', ('libc.so.6',))
        _shared_object(self.driver / 'libnvidia-ml.so.1', 'libnvidia-ml.so.1', ('libc.so.6',))
        self.gnu_bounds = driver_closure.TreeBounds(32, 8, 1_000_000, 2_000_000)
        self.driver_bounds = driver_closure.TreeBounds(32, 8, 1_000_000, 2_000_000)

    def tearDown(self):
        self.temp.cleanup()

    def inspect(self, entries=('libcuda.so.1',)):
        return driver_closure.inspect_cuda_driver_closure(
            self.gnu, self.driver,
            gnu_bounds=self.gnu_bounds,
            driver_bounds=self.driver_bounds,
            entry_sonames=entries,
        )

    def test_default_remains_cuda_only_and_worker_alias_is_preserved(self):
        closure = driver_closure.inspect_cuda_driver_closure(
            self.gnu, self.driver,
            gnu_bounds=self.gnu_bounds,
            driver_bounds=self.driver_bounds,
        )
        self.assertIn('libcuda.so.1', closure.providers)
        self.assertEqual(closure.providers['cuda'], self.driver / 'libcuda.so.1')
        self.assertNotIn('libnvidia-ml.so.1', closure.providers)

    def test_qualification_traverses_both_entry_graphs(self):
        closure = self.inspect(('libcuda.so.1', 'libnvidia-ml.so.1'))
        self.assertIn('libcuda.so.1', closure.providers)
        self.assertIn('libnvidia-ml.so.1', closure.providers)
        self.assertEqual(closure.providers['cuda'], self.driver / 'libcuda.so.1')
        self.assertEqual(closure.providers['nvml'], self.driver / 'libnvidia-ml.so.1')
        self.assertEqual(closure.providers['libc.so.6'], self.gnu / 'libc.so.6')

    def test_missing_or_invalid_second_entry_refuses(self):
        (self.driver / 'libnvidia-ml.so.1').unlink()
        with self.assertRaisesRegex(driver_closure.DriverClosureError, 'libnvidia-ml.so.1'):
            self.inspect(('libcuda.so.1', 'libnvidia-ml.so.1'))
        for names in ((), ('libcuda.so.1', 'libcuda.so.1'), ('../libcuda.so.1',)):
            with self.subTest(names=names):
                with self.assertRaisesRegex(driver_closure.DriverClosureError, 'entry SONAME set'):
                    self.inspect(names)

    def test_ambiguous_required_provider_refuses(self):
        duplicate = self.driver / 'libnvidia-ml-copy.so'
        _shared_object(duplicate, 'libnvidia-ml.so.1', ('libc.so.6',))
        with self.assertRaisesRegex(driver_closure.DriverClosureError, 'ambiguous'):
            self.inspect(('libcuda.so.1', 'libnvidia-ml.so.1'))


if __name__ == '__main__':
    unittest.main()
