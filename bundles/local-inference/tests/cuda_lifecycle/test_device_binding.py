"""New ordinal/UUID controls only; no replay of the existing passed suite.

Real pinned generated CUDA bindings and CUDADevice use injected native functions;
no native library, GPU, model, compiler child or provider operation occurs.
"""
from __future__ import annotations

import ctypes
import importlib
import os
from pathlib import Path
import sys
import unittest
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / '.ai/workers/local-inference/lib/local-tinygrad-gnu'))
from cuda_lifecycle import LifecycleError, install
from cuda_lifecycle.device_binding import DeviceBindingError, qualify_ordinal_zero
import test_pinned_integration as pinned
from test_pinned_integration import Driver, PINNED

UUID = bytes.fromhex('00112233445566778899aabbccddeeff')


class DeviceDriver(Driver):
    def __init__(self):
        super().__init__()
        self.visible_count = 1
        self.device_handle = 37  # an opaque CUdevice, not its ordinal zero
        self.uuid = UUID
        self.capability_handle = None
        self.uuid_handle = None
        self.context_handle = None
        self.status_overrides = {}

    def call(self, name, argtypes, args):
        if name == 'cuInit' and name in self.status_overrides:
            self.calls.append(name)
            return self.status_overrides[name]
        if name in {'cuDeviceGetCount', 'cuDeviceGet', 'cuDeviceGetUuid_v2', 'cuDeviceComputeCapability'}:
            self.calls.append(name)
            if self.fail == name: return 999
            if name == 'cuDeviceGetCount':
                ctypes.cast(args[0], ctypes.POINTER(ctypes.c_int))[0] = self.visible_count
            elif name == 'cuDeviceGet':
                if args[1] != 0: return 999
                ctypes.cast(args[0], ctypes.POINTER(ctypes.c_int))[0] = self.device_handle
            elif name == 'cuDeviceGetUuid_v2':
                self.uuid_handle = args[1].value
                if self.uuid_handle != self.device_handle: return 999
                ctypes.memmove(args[0], self.uuid, 16)
            else:
                self.capability_handle = args[2].value if hasattr(args[2], 'value') else args[2]
                if self.capability_handle != self.device_handle: return 999
                ctypes.cast(args[0], ctypes.POINTER(ctypes.c_int))[0] = 8
                ctypes.cast(args[1], ctypes.POINTER(ctypes.c_int))[0] = 0
            return self.status_overrides.get(name, 0)
        if name == 'cuCtxCreate_v2':
            self.context_handle = args[2].value
            if self.context_handle != self.device_handle: return 999
        return super().call(name, argtypes, args)


class DeviceBindingTests(unittest.TestCase):
    def setUp(self):
        pinned.PinnedIntegrationTests.setUpClass()  # require exact source hashes
        for name in tuple(sys.modules):
            if name == 'tinygrad' or name.startswith('tinygrad.'): del sys.modules[name]
        self.path = mock.patch.object(sys, 'path', [str(PINNED), *sys.path])
        self.path.start(); self.addCleanup(self.path.stop)
        self.env = mock.patch.dict(os.environ, {'DEV':'CUDA:PTX','JIT':'0','PROFILE':'0',
                                              'DEBUG':'0','BEAM':'0','CACHELEVEL':'0','CCACHE':'0'}, clear=True)
        self.env.start(); self.addCleanup(self.env.stop)
        from tinygrad.runtime.support import c
        self.driver = DeviceDriver()
        self.dll = mock.patch.object(c, 'DLL', self.driver.dll_class())
        self.dll.start(); self.addCleanup(self.dll.stop)
        self.no_native = mock.patch('ctypes.CDLL', side_effect=AssertionError('native loading prohibited'))
        self.no_native.start(); self.addCleanup(self.no_native.stop)
        self.bindings = importlib.import_module('tinygrad.runtime.autogen.cuda')

    def query(self, expected=UUID.hex()):
        return qualify_ordinal_zero(bindings=self.bindings, expected_uuid_v2_hex=expected)

    def test_real_generated_uuid_v2_query_preserves_embedded_nul_and_opaque_handle(self):
        result = self.query()
        self.assertEqual((result.visible_device_count, result.selected_ordinal, result.uuid_v2_hex), (1, 0, UUID.hex()))
        self.assertEqual(self.driver.uuid_handle, 37)
        self.assertEqual(self.driver.calls, ['cuInit', 'cuDeviceGetCount', 'cuDeviceGet', 'cuDeviceGetUuid_v2'])
        self.assertNotIn('cuCtxCreate_v2', self.driver.calls)

    def test_missing_malformed_or_zero_expected_identity_refuses_before_native_use(self):
        for expected in (None, '', 'GPU-' + UUID.hex(), UUID.hex().upper(), '0'*32, '1'*64):
            with self.subTest(expected=expected), self.assertRaises(DeviceBindingError): self.query(expected)
        self.assertFalse(self.driver.calls)

    def test_no_device_and_extra_visible_device_refuse_before_selection_or_context(self):
        for count in (0, 2, -1):
            self.driver.calls.clear()
            self.driver.visible_count = count
            with self.subTest(count=count), self.assertRaises(DeviceBindingError): self.query()
            self.assertEqual(self.driver.calls, ['cuInit', 'cuDeviceGetCount'])

    def test_uuid_mismatch_does_not_fallback_to_pci_name_or_old_uuid(self):
        self.driver.uuid = bytes.fromhex('f'*32)
        with self.assertRaises(DeviceBindingError): self.query()
        self.assertEqual(self.driver.calls, ['cuInit', 'cuDeviceGetCount', 'cuDeviceGet', 'cuDeviceGetUuid_v2'])
        self.assertNotIn('cuCtxCreate_v2', self.driver.calls)

    def test_query_errors_and_absent_uuid_v2_never_return_a_match(self):
        for name in ('cuInit','cuDeviceGetCount','cuDeviceGet','cuDeviceGetUuid_v2'):
            self.driver.calls.clear(); self.driver.fail = name
            with self.subTest(name=name), self.assertRaises(DeviceBindingError): self.query()
            self.assertEqual(self.driver.calls[-1], name)
            self.assertNotIn('cuCtxCreate_v2', self.driver.calls)
        self.driver.calls.clear()
        with mock.patch.object(self.bindings, 'cuDeviceGetUuid_v2', create=False) as method:
            del self.bindings.cuDeviceGetUuid_v2
            with self.assertRaises(DeviceBindingError): self.query()
        self.assertFalse(self.driver.calls)

    def test_pinned_device_context_and_capability_use_actual_handle_not_ordinal(self):
        observations = []
        lifecycle = install(bindings=self.bindings, observe=observations.append)
        self.addCleanup(lifecycle.shutdown)
        from tinygrad.device import Device
        device = Device['CUDA']
        self.assertEqual(device.cu_device.value, 37)
        self.assertEqual(self.driver.context_handle, 37)
        self.assertEqual(self.driver.capability_handle, 37)
        self.assertEqual(lifecycle.shutdown().status, 'closed')
        self.assertEqual(len(observations), 1)
        with self.assertRaises(LifecycleError): device.synchronize()

    def test_guarded_same_handle_uuid_v2_precedes_context_with_embedded_zero(self):
        observations = []
        lifecycle = install(bindings=self.bindings, observe=observations.append,
                            expected_uuid_v2_hex=UUID.hex())
        self.addCleanup(lifecycle.shutdown)
        from tinygrad.device import Device
        device = Device['CUDA']
        self.assertEqual(device.cu_device.value, 37)
        self.assertEqual(self.driver.uuid_handle, 37)
        self.assertEqual(self.driver.context_handle, 37)
        self.assertLess(self.driver.calls.index('cuDeviceGetUuid_v2'),
                        self.driver.calls.index('cuCtxCreate_v2'))
        self.assertEqual(lifecycle.shutdown().status, 'closed')
        self.assertEqual(len(observations), 1)

    def test_guarded_uuid_mismatch_refuses_before_context_or_retired(self):
        self.driver.uuid = bytes.fromhex('f' * 32)
        observations = []
        lifecycle = install(bindings=self.bindings, observe=observations.append,
                            expected_uuid_v2_hex=UUID.hex())
        self.addCleanup(lifecycle.shutdown)
        from tinygrad.device import Device
        with self.assertRaises(LifecycleError): Device['CUDA']
        self.assertEqual(self.driver.uuid_handle, 37)
        self.assertNotIn('cuCtxCreate_v2', self.driver.calls)
        self.assertEqual(lifecycle.inventory()['context'], 0)
        self.assertEqual(lifecycle.shutdown().status, 'failed')
        self.assertFalse(observations)

    def test_guarded_uuid_query_error_refuses_before_context_or_retired(self):
        self.driver.fail = 'cuDeviceGetUuid_v2'
        observations = []
        lifecycle = install(bindings=self.bindings, observe=observations.append,
                            expected_uuid_v2_hex=UUID.hex())
        self.addCleanup(lifecycle.shutdown)
        from tinygrad.device import Device
        with self.assertRaises(LifecycleError): Device['CUDA']
        self.assertIn('cuDeviceGetUuid_v2', self.driver.calls)
        self.assertNotIn('cuCtxCreate_v2', self.driver.calls)
        self.assertEqual(lifecycle.shutdown().status, 'failed')
        self.assertFalse(observations)

    def test_guarded_uuid_parameter_and_public_query_do_not_grant_native_access(self):
        for value in ('', '0' * 32, UUID.hex().upper(), 'GPU-' + UUID.hex()):
            with self.subTest(value=value), self.assertRaises(LifecycleError):
                install(bindings=self.bindings, expected_uuid_v2_hex=value)
        self.assertFalse(self.driver.calls)
        lifecycle = install(bindings=self.bindings, expected_uuid_v2_hex=UUID.hex())
        self.addCleanup(lifecycle.shutdown)
        with self.assertRaises(LifecycleError):
            self.bindings.cuDeviceGetUuid_v2(None, self.bindings.CUdevice(37))
        self.assertFalse(self.driver.calls)
        self.assertEqual(lifecycle.shutdown().status, 'failed')

    def test_opt_in_count_qualification_is_guarded_before_device_and_preserves_tinygrad_cuinit(self):
        observations = []
        lifecycle = install(bindings=self.bindings, observe=observations.append,
                            expected_uuid_v2_hex=UUID.hex(), require_single_visible_device=True)
        self.assertFalse(self.driver.calls)
        lifecycle.qualify_single_visible_device()
        self.assertEqual(self.driver.calls, ['cuInit', 'cuDeviceGetCount'])
        from tinygrad.device import Device
        device = Device['CUDA']
        self.assertEqual(device.cu_device.value, 37)
        self.assertEqual(self.driver.calls.count('cuDeviceGetCount'), 1)
        self.assertEqual(self.driver.calls.count('cuInit'), 2)
        self.assertLess(self.driver.calls.index('cuDeviceGetCount'), self.driver.calls.index('cuDeviceGet'))
        self.assertLess(self.driver.calls.index('cuDeviceGetUuid_v2'), self.driver.calls.index('cuCtxCreate_v2'))
        self.assertEqual(lifecycle.shutdown().status, 'closed')
        self.assertEqual(len(observations), 1)
        calls_at_close = list(self.driver.calls)
        with self.assertRaises(LifecycleError): lifecycle.qualify_single_visible_device()
        self.assertEqual(self.driver.calls, calls_at_close)

    def test_opt_in_count_gate_blocks_device_acquisition_until_success(self):
        observations = []
        lifecycle = install(bindings=self.bindings, observe=observations.append,
                            expected_uuid_v2_hex=UUID.hex(), require_single_visible_device=True)
        from tinygrad.device import Device
        with self.assertRaises(LifecycleError): Device['CUDA']
        self.assertIn('cuInit', self.driver.calls)
        self.assertNotIn('cuDeviceGet', self.driver.calls)
        self.assertNotIn('cuCtxCreate_v2', self.driver.calls)
        self.assertEqual(lifecycle.shutdown().status, 'failed')
        self.assertFalse(observations)

    def test_opt_in_count_mismatch_is_sticky_and_never_reaches_device_or_retired(self):
        self.driver.visible_count = 2
        observations = []
        lifecycle = install(bindings=self.bindings, observe=observations.append,
                            expected_uuid_v2_hex=UUID.hex(), require_single_visible_device=True)
        with self.assertRaises(LifecycleError): lifecycle.qualify_single_visible_device()
        calls_at_failure = list(self.driver.calls)
        self.assertEqual(calls_at_failure, ['cuInit', 'cuDeviceGetCount'])
        with self.assertRaises(LifecycleError): lifecycle.qualify_single_visible_device()
        self.assertEqual(self.driver.calls, calls_at_failure)
        self.assertNotIn('cuDeviceGet', self.driver.calls)
        self.assertNotIn('cuCtxCreate_v2', self.driver.calls)
        self.assertEqual(lifecycle.shutdown().status, 'failed')
        self.assertFalse(observations)

    def assert_count_qualification_failure(self, failure, override=None):
        observations = []
        if override is None:
            self.driver.fail = failure
        else:
            self.driver.status_overrides[failure] = override
        lifecycle = install(bindings=self.bindings, observe=observations.append,
                            expected_uuid_v2_hex=UUID.hex(), require_single_visible_device=True)
        with self.assertRaises(LifecycleError): lifecycle.qualify_single_visible_device()
        calls_at_failure = list(self.driver.calls)
        with self.assertRaises(LifecycleError): lifecycle.qualify_single_visible_device()
        self.assertEqual(self.driver.calls, calls_at_failure)
        self.assertNotIn('cuDeviceGet', self.driver.calls)
        self.assertNotIn('cuCtxCreate_v2', self.driver.calls)
        self.assertEqual(lifecycle.shutdown().status, 'failed')
        self.assertFalse(observations)

    def test_opt_in_count_init_error_is_sticky(self):
        self.assert_count_qualification_failure('cuInit')

    def test_opt_in_count_query_error_is_sticky(self):
        self.assert_count_qualification_failure('cuDeviceGetCount')

    def test_opt_in_count_noninteger_status_is_sticky(self):
        self.assert_count_qualification_failure('cuInit', False)

    def test_opt_in_count_query_noninteger_status_is_sticky(self):
        self.assert_count_qualification_failure('cuDeviceGetCount', False)

    def test_qualifier_never_escapes_installed_lifecycle_or_enables_extra_queries(self):
        lifecycle = install(bindings=self.bindings)
        self.addCleanup(lifecycle.shutdown)
        with self.assertRaises(DeviceBindingError): self.query()
        self.assertFalse(self.driver.calls)
        with self.assertRaises(LifecycleError): self.bindings.cuDeviceGetUuid_v2(None, 37)
        self.assertFalse(self.driver.calls)


if __name__ == '__main__': unittest.main()
