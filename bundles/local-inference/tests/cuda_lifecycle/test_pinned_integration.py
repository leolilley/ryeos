"""Offline real pinned Python paths; generated bindings have an injected DLL.

Harness isolation/imports/threads are test setup, not product supervisors. Never
load a native library, execute compiler children, model weights or provider code.
Set RYEOS_TEST_TINYGRAD_ROOT to the selected read-only source checkout.
"""
from __future__ import annotations

import atexit
import ctypes
import gc
import hashlib
import json
import importlib
import os
from pathlib import Path
import sys
import threading
import unittest
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / '.ai/workers/local-inference/lib/local-tinygrad-gnu'
PINNED = Path(os.environ.get('RYEOS_TEST_TINYGRAD_ROOT', '/home/leo/projects/vendor/tinygrad'))
sys.path.insert(0, str(SOURCE))
from cuda_lifecycle import install, LifecycleError


class Driver:
    def __init__(self):
        self.calls = []
        self.fail = None
        self.failures = {}
        self.objects = []
        self.allocations = {}
        self.next_handle = 100
        self.after = None
        self.visible_devices = 1
        self.device_handle = 7
        self.device_uuid = bytes.fromhex('00112233445566778899aabbccddeeff')
        self.uuid_query_handles = []
        self.context_device_handles = []

    def call(self, name, argtypes, args):
        self.calls.append(name)
        failure = self.failures.get(name)
        if isinstance(failure, BaseException): raise failure
        if failure is not None: return failure
        if self.fail == name: return 999
        if name == 'cuDeviceGetCount':
            ctypes.cast(args[0], ctypes.POINTER(ctypes.c_int))[0] = self.visible_devices
        elif name == 'cuDeviceGet': ctypes.cast(args[0], ctypes.POINTER(ctypes.c_int))[0] = self.device_handle
        elif name == 'cuDeviceGetUuid_v2':
            handle = getattr(args[1], 'value', args[1])
            self.uuid_query_handles.append(handle)
            ctypes.memmove(args[0], self.device_uuid, 16)
        elif name == 'cuDeviceComputeCapability':
            self.context_device_handles.append(getattr(args[2], 'value', args[2]))
            ctypes.cast(args[0], ctypes.POINTER(ctypes.c_int))[0] = 8
            ctypes.cast(args[1], ctypes.POINTER(ctypes.c_int))[0] = 0
        elif name in ('cuCtxCreate_v2', 'cuModuleLoadData', 'cuModuleGetFunction', 'cuEventCreate'):
            if name == 'cuCtxCreate_v2':
                self.context_device_handles.append(getattr(args[2], 'value', args[2]))
            self.next_handle += 1
            ctypes.cast(args[0], ctypes.POINTER(ctypes.c_void_p))[0] = self.next_handle
        elif name in ('cuMemAlloc_v2', 'cuMemHostAlloc'):
            allocation = ctypes.create_string_buffer(args[1])
            self.objects.append(allocation)
            ptr = ctypes.addressof(allocation)
            self.allocations[ptr] = allocation
            ctypes.cast(args[0], argtypes[0])[0] = ptr
        elif name in ('cuMemFree_v2', 'cuMemFreeHost'):
            self.allocations.pop(getattr(args[0], 'value', args[0]), None)
        elif name == 'cuMemcpyHtoDAsync_v2': ctypes.memmove(args[0].value, args[1].value, args[2])
        elif name == 'cuMemcpyDtoH_v2': ctypes.memmove(args[0], args[1].value, args[2])
        elif name == 'cuEventElapsedTime': ctypes.cast(args[0], ctypes.POINTER(ctypes.c_float))[0] = 1
        if self.after is not None: self.after(name)
        return 0

    def dll_class(self):
        driver = self
        class InjectedDLL:
            _loaded_ = set()
            def __init__(self, *args, **kwargs): pass
            def bind(self, restype, *argtypes):
                def bind(fn):
                    def call(*args): return driver.call(fn.__name__, argtypes, args)
                    call.argtypes, call.restype = argtypes, restype
                    return call
                return bind
        return InjectedDLL


class PinnedIntegrationTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        pin = json.loads(Path(__file__).with_name('source_pin.json').read_text())
        for member, expected in pin['files'].items():
            actual = hashlib.sha256((PINNED / member).read_bytes()).hexdigest()
            if actual != expected:
                raise AssertionError(f'selected tinygrad source differs: {member}')

    def setUp(self):
        # Reimport the REAL upstream code per test to isolate installed hooks.
        # No mutation of the vendor checkout is performed.
        gc.collect()
        for name in list(sys.modules):
            if name == 'tinygrad' or name.startswith('tinygrad.'): del sys.modules[name]
        self.path = mock.patch.object(sys, 'path', [str(PINNED), *sys.path])
        self.path.start(); self.addCleanup(self.path.stop)
        self.env = mock.patch.dict(os.environ, {'DEV':'CUDA:PTX', 'JIT':'0', 'PROFILE':'0',
                                              'DEBUG':'0', 'BEAM':'0', 'CACHELEVEL':'0', 'CCACHE':'0'}, clear=True)
        self.env.start(); self.addCleanup(self.env.stop)
        self.no_native = mock.patch('ctypes.CDLL', side_effect=AssertionError('native DLL loading forbidden'))
        # support.c subclasses CDLL, so import it BEFORE replacing that type;
        # replace DLL itself before any generated binding can instantiate it.
        self.exit_callbacks = []
        original_register = atexit.register
        def register(fn, *args, **kwargs):
            self.exit_callbacks.append(fn)
            return original_register(fn, *args, **kwargs)
        self.exit_patch = mock.patch.object(atexit, 'register', register)
        self.exit_patch.start(); self.addCleanup(self.exit_patch.stop)
        from tinygrad.runtime.support import c
        self.driver = Driver()
        self.dll = mock.patch.object(c, 'DLL', self.driver.dll_class())
        self.dll.start(); self.addCleanup(self.dll.stop)
        self.no_native.start(); self.addCleanup(self.no_native.stop)
        self.observations = []
        self.require_single_visible_device = bool(getattr(self, 'require_single_visible_device', False))
        self.expected_uuid_v2_hex = '00112233445566778899aabbccddeeff'
        self.lifecycle = install(
            observe=self.observations.append,
            expected_uuid_v2_hex=self.expected_uuid_v2_hex if self.require_single_visible_device else None,
            require_single_visible_device=self.require_single_visible_device,
        )
        from tinygrad.runtime import ops_cuda
        from tinygrad.device import Buffer, BufferSpec, Device, TinyELF
        from tinygrad.dtype import dtypes
        from tinygrad.helpers import Target
        self.ops, self.Buffer, self.Spec, self.Device = ops_cuda, Buffer, BufferSpec, Device
        self.TinyELF, self.dtypes, self.Target = TinyELF, dtypes, Target
        self.cuda = importlib.import_module('tinygrad.runtime.autogen.cuda')
        self.addCleanup(self.cleanup)

    def cleanup(self):
        self.lifecycle.shutdown()
        # Device.finalize is inert in this admitted subset. Remove upstream
        # exit callbacks for isolated imports to avoid accumulating harness state.
        for fn in getattr(self, 'exit_callbacks', []): atexit.unregister(fn)

    def device(self): return self.Device['CUDA']

    def program(self, device):
        obj = self.TinyELF(b'.version 7.5\n.target sm_80\n', 'test', self.Target('CUDA'), ())
        return self.ops.CUDAProgram(device, obj)

    def test_real_device_allocator_program_copies_cache_and_terminal_alias(self):
        with self.lifecycle.submission():
            device = self.device()
            buf = self.Buffer('CUDA', 4, self.dtypes.uint8).allocate()
            device.allocator._copyin(buf._buf, memoryview(bytearray(b'abcd')))
            host = device.pending_copyin[0][0]
            device.synchronize()
            self.assertTrue(any(host in xs for xs in device.allocator.cache.values()))
            out = memoryview(bytearray(4))
            device.allocator._copyout(out, buf._buf)
            self.assertEqual(bytes(out), b'abcd')
            program = self.program(device)
            program(wait=True)  # real timed launch helper creates/destroys events
            alias = self.cuda.cuLaunchKernel
            buf.deallocate()  # real LRU cache retains allocation
        self.assertEqual(self.lifecycle.inventory()['host'], 1)
        self.assertEqual(self.lifecycle.inventory()['device'], 1)
        result = self.lifecycle.shutdown()
        self.assertEqual(result.status, 'closed')
        self.assertTrue(result.observation_delivered)
        self.assertEqual(len(self.observations), 1)
        calls = self.driver.calls
        last_sync = len(calls) - 1 - calls[::-1].index('cuCtxSynchronize')
        self.assertLess(last_sync, calls.index('cuModuleUnload'))
        self.assertLess(calls.index('cuModuleUnload'), calls.index('cuMemFree_v2'))
        self.assertLess(calls.index('cuMemFree_v2'), calls.index('cuMemFreeHost'))
        self.assertLess(calls.index('cuMemFreeHost'), calls.index('cuCtxDestroy_v2'))
        n = len(calls)
        self.assertIs(self.lifecycle.shutdown(), result)
        program.__del__(); device.finalize()
        for fn in self.exit_callbacks:
            if fn.__module__ == 'tinygrad.device': fn()  # real upstream atexit device callback
        del program; del buf; gc.collect()
        with self.assertRaises(LifecycleError): alias(None)
        with self.assertRaises(LifecycleError): device.synchronize()
        with self.assertRaises(LifecycleError): device.allocator.alloc(4)
        self.assertEqual(len(calls), n)
        self.assertFalse(self.driver.allocations)

    def test_offset_view_stays_in_owned_allocation(self):
        device = self.device()
        ptr = device.allocator.alloc(8)
        view = device.allocator._offset(ptr, 4, 4)
        self.assertEqual(view.value, ptr.value + 4)
        with self.assertRaises(LifecycleError): device.allocator._offset(ptr, 4, 8)
        self.assertEqual(self.lifecycle.shutdown().status, 'failed')
        self.assertFalse(self.observations)

    def test_timed_launch_partial_event_acquisition_keeps_real_inventory(self):
        device = self.device()
        program = self.program(device)
        def fail_second(name):
            if name == 'cuEventCreate': self.driver.fail = 'cuEventCreate'
        self.driver.after = fail_second
        with self.assertRaises(LifecycleError): program(wait=True)
        self.assertEqual(self.lifecycle.inventory()['event'], 1)
        self.assertNotIn('cuLaunchKernel', self.driver.calls)
        self.assertEqual(self.lifecycle.shutdown().status, 'failed')
        self.assertFalse(self.observations)

    def test_timed_launch_checks_upstream_ignored_event_record_error(self):
        device = self.device()
        program = self.program(device)
        self.driver.fail = 'cuEventRecord'
        with self.assertRaises(LifecycleError): program(wait=True)
        self.assertEqual(self.lifecycle.inventory()['event'], 2)
        self.assertEqual(self.lifecycle.shutdown().status, 'failed')
        self.assertFalse(self.observations)

    def test_real_pending_host_buffer_without_synchronize_is_explicitly_freed(self):
        device = self.device()
        ptr = device.allocator.alloc(8)
        device.allocator._copyin(ptr, memoryview(bytearray(8)))
        self.assertEqual(len(device.pending_copyin), 1)
        self.assertEqual(self.lifecycle.shutdown().status, 'closed')
        self.assertIn('cuMemFreeHost', self.driver.calls)
        self.assertEqual(self.observations[0].host_allocations_freed, 1)

    def test_forbidden_adoption_preserves_cpu_from_blob(self):
        self.device()
        for kwargs in ({'opaque':ctypes.c_uint64(123)}, {'external_ptr':123}):
            with self.assertRaises(LifecycleError): self.Buffer('CUDA', 1, self.dtypes.uint8).allocate(**kwargs)
        with self.assertRaises(LifecycleError):
            self.Buffer('CUDA', 1, self.dtypes.uint8, options=self.Spec(external_ptr=0)).allocate()
        with self.assertRaises((MemoryError, LifecycleError)):
            self.device().allocator.alloc(1, self.Spec(external_ptr=123))
        # CPU from_blob uses the real CPU Buffer path but no compiler or libc
        # operation: external_ptr is forwarded directly by CPUAllocator._alloc.
        from tinygrad import Tensor
        data = ctypes.create_string_buffer(b'xy')
        # CPUDevice construction compiles native service programs and opens a
        # semaphore. Inject that initialization; exercise actual from_blob,
        # Buffer.allocate and CPUAllocator._alloc without those host effects.
        with mock.patch('ctypes.CDLL', side_effect=OSError('offline no library')):
            from tinygrad.runtime.ops_cpu import CPUDevice, CPUAllocator
        from tinygrad.device import Compiled, LRUAllocator
        def cpu_init(device, name):
            allocator = CPUAllocator.__new__(CPUAllocator)
            LRUAllocator.__init__(allocator, device)
            Compiled.__init__(device, name, allocator, [], None)
            device.ring_pos = 0
            device.synchronize = lambda: None
        with mock.patch.object(CPUDevice, '__init__', cpu_init):
            tensor = Tensor.from_blob(ctypes.addressof(data), (2,), dtype=self.dtypes.uint8, device='CPU')
        self.assertEqual(tensor.uop.buffer.options.external_ptr, ctypes.addressof(data))

    def test_extra_device_graph_external_api_transfer_refuse_without_native(self):
        device = self.device()
        n = len(self.driver.calls)
        from tinygrad.runtime.graph.cuda import CUDAGraph
        for action in (lambda: self.ops.CUDADevice('CUDA:1'), lambda: self.ops.CUDADevice('CUDA'),
                       lambda: self.Device['NV'], lambda: self.Device['CUDA:1'],
                       lambda: self.ops.CUDAAllocator(device).alloc(4),
                       lambda: CUDAGraph(None), lambda: device.allocator._transfer(None, None, 1, device, device),
                       lambda: self.cuda.cuMemAllocAsync(None, 1, None)):
            with self.assertRaises(LifecycleError): action()
        self.assertEqual(len(self.driver.calls), n)
        self.assertEqual(self.lifecycle.shutdown().status, 'failed')
        self.assertFalse(self.observations)

    def test_context_acquisition_survives_failure_before_registration(self):
        self.driver.fail = 'cuDeviceComputeCapability'
        with self.assertRaises(LifecycleError): self.device()
        self.assertEqual(self.ops.CUDADevice.devices, [])
        self.assertEqual(self.lifecycle.inventory()['context'], 1)
        self.assertEqual(self.lifecycle.shutdown().status, 'failed')
        self.assertFalse(self.observations)

    def test_context_acquisition_survives_compiled_initialization_failure(self):
        from tinygrad.device import Compiled
        with mock.patch.object(Compiled, '__init__', side_effect=ValueError('constructor failed')):
            with self.assertRaises(ValueError): self.device()
        self.assertEqual(self.lifecycle.inventory()['context'], 1)
        self.assertEqual(self.lifecycle.shutdown().status, 'failed')
        self.assertFalse(self.observations)

    def test_module_acquisition_survives_function_lookup_failure(self):
        device = self.device()
        self.driver.fail = 'cuModuleGetFunction'
        with self.assertRaises(LifecycleError): self.program(device)
        gc.collect()
        self.assertEqual(self.lifecycle.shutdown().status, 'failed')
        self.assertFalse(self.observations)
        # Upstream partial object's destructor may already unload its module;
        # either retained or checked successful unload, never inferred success.
        self.assertEqual(self.lifecycle.inventory()['module'] + self.driver.calls.count('cuModuleUnload'), 1)

    def _allocation_helper_failure(self, options, kind):
        device = self.device()
        original = self.ops.init_c_var
        def fail_after(typ, callback):
            value = original(typ, callback)
            if typ in (ctypes.c_void_p, self.cuda.CUdeviceptr): raise ValueError('after native acquisition')
            return value
        with mock.patch.object(self.ops, 'init_c_var', fail_after):
            with self.assertRaises(ValueError): device.allocator._alloc(4, options)
            self.assertEqual(self.lifecycle.inventory()[kind], 1)
        self.assertEqual(self.lifecycle.shutdown().status, 'failed')
        self.assertFalse(self.observations)

    def test_host_acquisition_survives_python_helper_failure(self):
        self._allocation_helper_failure(self.Spec(host=True), 'host')

    def test_device_acquisition_survives_python_helper_failure(self):
        self._allocation_helper_failure(self.Spec(), 'device')

    def test_buffer_registration_failure_retains_actual_device_allocation(self):
        device = self.device()
        original = device.allocator.alloc
        def fail_after(*args, **kwargs):
            original(*args, **kwargs)
            raise ValueError('after allocator before Buffer registration')
        with mock.patch.object(device.allocator, 'alloc', fail_after):
            with self.assertRaises(ValueError): self.Buffer('CUDA', 4, self.dtypes.uint8).allocate()
        self.assertEqual(self.lifecycle.inventory()['device'], 1)
        self.assertEqual(self.lifecycle.shutdown().status, 'failed')
        self.assertFalse(self.observations)

    def test_errors_synchronize_host_free_device_free_module_event_destroy_context_destroy(self):
        # One test per injected error needs a fresh installed runtime. Subtests
        # below use independent test instances, without invoking real libraries.
        for failure in ('cuCtxSynchronize','cuMemFreeHost','cuMemFree_v2','cuModuleUnload','cuEventDestroy_v2','cuCtxDestroy_v2'):
            with self.subTest(failure=failure):
                if failure != 'cuCtxSynchronize':
                    self.doCleanups(); self.setUp()
                device = self.device()
                device.allocator.alloc(4)
                device.allocator.alloc(4, self.Spec(host=True))
                program = self.program(device)
                event = self.cuda.CUevent()
                self.cuda.cuEventCreate(ctypes.byref(event), 0)
                self.driver.fail = failure
                result = self.lifecycle.shutdown()
                self.assertEqual(result.status, 'failed')
                self.assertFalse(result.observation_delivered)
                self.assertFalse(self.observations)
                self.assertTrue(any(self.lifecycle.inventory().values()))
                n = len(self.driver.calls)
                self.assertIs(self.lifecycle.shutdown(), result)
                program.__del__()
                with self.assertRaises(LifecycleError): self.cuda.cuCtxSynchronize()
                self.assertEqual(len(self.driver.calls), n)

    def test_shutdown_closes_new_requests_while_existing_request_finishes(self):
        self.device()
        entered, finish = threading.Event(), threading.Event()
        errors = []
        def request():
            try:
                with self.lifecycle.submission():
                    entered.set(); finish.wait(2)
                    self.device().synchronize()  # nested admitted work after close
            except BaseException as error: errors.append(error)
        worker = threading.Thread(target=request)
        worker.start()
        try:
            self.assertTrue(entered.wait(2))
            self.assertEqual(self.lifecycle.shutdown().status, 'busy')
            with self.assertRaises(LifecycleError):
                with self.lifecycle.submission(): pass
            finish.set(); worker.join(2)
            self.assertFalse(worker.is_alive())
            self.assertFalse(errors)
            self.assertEqual(self.lifecycle.shutdown().status, 'closed')
        finally:
            finish.set(); worker.join(2)

    def test_missing_observer_and_delivery_error_never_report_delivered(self):
        self.device()
        self.lifecycle._observe = None
        result = self.lifecycle.shutdown()
        self.assertEqual(result.status, 'closed')
        self.assertFalse(result.observation_delivered)
        self.assertFalse(self.observations)

    def test_callback_runs_after_terminal_fence_and_never_replays_on_failure(self):
        self.device()
        def observer(observation):
            with self.assertRaises(LifecycleError): self.cuda.cuCtxSynchronize()
            self.assertEqual(self.lifecycle.shutdown().status, 'closed')
            raise ValueError('delivery failed')
        self.lifecycle._observe = observer
        result = self.lifecycle.shutdown()
        self.assertEqual(result.status, 'failed')
        self.assertFalse(result.observation_delivered)
        n = len(self.driver.calls)
        self.assertIs(self.lifecycle.shutdown(), result)
        self.assertEqual(len(self.driver.calls), n)

    def test_mutated_profile_cannot_create_events_or_emit_success(self):
        self.device()
        from tinygrad import helpers
        helpers.PROFILE.value = 1
        n = len(self.driver.calls)
        with self.assertRaises(LifecycleError): self.cuda.cuCtxSynchronize()
        self.assertEqual(self.lifecycle.shutdown().status, 'failed')
        self.assertEqual(len(self.driver.calls), n)
        self.assertFalse(self.observations)

    def test_replaced_binding_cannot_emit_success(self):
        self.device()
        self.cuda.cuLaunchKernel = lambda *args: 0
        self.assertEqual(self.lifecycle.shutdown().status, 'failed')
        self.assertFalse(self.observations)

    def test_real_tensor_ptx_compile_realize_and_runtime_cache_path(self):
        # Real tensor graph, compiler and realization; injected kernel does no
        # arithmetic, so this establishes path coverage, NOT numerical accuracy.
        from tinygrad import Tensor
        from tinygrad.engine.realize import runtime_cache
        tensor = (Tensor([1., 2.]) + 3.).contiguous().realize()
        self.assertTrue(runtime_cache)
        self.assertIn('cuModuleLoadData', self.driver.calls)
        self.assertIn('cuLaunchKernel', self.driver.calls)
        self.assertEqual(self.lifecycle.shutdown().status, 'closed')
        n = len(self.driver.calls)
        runtime_cache.clear(); del tensor; gc.collect()
        self.assertEqual(len(self.driver.calls), n)


class LifecycleDeviceQualificationTests(unittest.TestCase):
    """Strict count/UUID gates around the real pinned Tinygrad CUDADevice path."""

    cleanup = PinnedIntegrationTests.cleanup
    device = PinnedIntegrationTests.device

    def setUp(self):
        PinnedIntegrationTests.setUpClass()
        self.require_single_visible_device = True
        PinnedIntegrationTests.setUp(self)

    def test_count_and_same_handle_uuid_precede_context_creation(self):
        self.lifecycle.qualify_single_visible_device()
        self.assertEqual(self.driver.calls, ['cuInit', 'cuDeviceGetCount'])
        self.device()
        # Tinygrad calls cuInit(0) again in CUDADevice.__init__; the lifecycle
        # acknowledges it without repeating native initialization or counting.
        self.assertEqual(self.driver.calls.count('cuInit'), 1)
        self.assertEqual(self.driver.calls.count('cuDeviceGetCount'), 1)
        self.assertEqual(self.driver.uuid_query_handles, [self.driver.device_handle])
        self.assertEqual(self.driver.context_device_handles, [self.driver.device_handle] * 2)
        self.assertLess(self.driver.calls.index('cuDeviceGetCount'), self.driver.calls.index('cuDeviceGet'))
        self.assertLess(self.driver.calls.index('cuDeviceGetUuid_v2'), self.driver.calls.index('cuCtxCreate_v2'))
        self.assertEqual(self.lifecycle.shutdown().status, 'closed')
        self.assertEqual(self.observations[0].contexts_destroyed, 1)

    def test_unqualified_driver_initialization_is_refused(self):
        with self.assertRaisesRegex(LifecycleError, 'count qualification must initialize'):
            self.cuda.cuInit(0)
        self.assertFalse(self.driver.calls)
        self.assertEqual(self.lifecycle.shutdown().status, 'failed')
        self.assertFalse(self.observations)

    def test_missing_count_binding_refuses_qualified_install(self):
        with self.assertRaisesRegex(LifecycleError, 'requires UUID-v2 comparison'):
            install(bindings=object(), require_single_visible_device=True)
        names = ('tinygrad.runtime.ops_cuda', 'tinygrad.runtime.graph.cuda')
        saved = {name: sys.modules.pop(name) for name in names if name in sys.modules}
        try:
            with self.assertRaisesRegex(LifecycleError, 'lacks visible-device count'):
                install(bindings=type('Bindings', (), {'cuDeviceGetCount': None})(),
                        expected_uuid_v2_hex=self.expected_uuid_v2_hex,
                        require_single_visible_device=True)
        finally:
            sys.modules.update(saved)

    def test_zero_visible_devices_fail_stickily_before_device_context(self):
        self.driver.visible_devices = 0
        with self.assertRaises(LifecycleError): self.lifecycle.qualify_single_visible_device()
        self.assertNotIn('cuDeviceGet', self.driver.calls)
        with self.assertRaises(LifecycleError): self.lifecycle.qualify_single_visible_device()
        self.assertEqual(self.driver.calls.count('cuDeviceGetCount'), 1)
        self.assertEqual(self.lifecycle.shutdown().status, 'failed')
        self.assertFalse(self.observations)

    def test_multiple_visible_devices_fail_stickily_before_device_context(self):
        self.driver.visible_devices = 2
        with self.assertRaises(LifecycleError): self.lifecycle.qualify_single_visible_device()
        self.assertNotIn('cuDeviceGet', self.driver.calls)
        self.assertEqual(self.lifecycle.shutdown().status, 'failed')
        self.assertFalse(self.observations)

    def test_failed_count_is_not_retried_or_treated_as_one_device(self):
        self.driver.fail = 'cuDeviceGetCount'
        with self.assertRaises(LifecycleError): self.lifecycle.qualify_single_visible_device()
        with self.assertRaises(LifecycleError): self.lifecycle.qualify_single_visible_device()
        self.assertEqual(self.driver.calls.count('cuDeviceGetCount'), 1)
        self.assertNotIn('cuDeviceGet', self.driver.calls)
        self.assertEqual(self.lifecycle.shutdown().status, 'failed')
        self.assertFalse(self.observations)

    def test_repeated_successful_count_query_poisoned_without_second_native_call(self):
        self.lifecycle.qualify_single_visible_device()
        with self.assertRaises(LifecycleError): self.lifecycle.qualify_single_visible_device()
        self.assertEqual(self.driver.calls.count('cuDeviceGetCount'), 1)
        self.assertNotIn('cuDeviceGet', self.driver.calls)
        self.assertEqual(self.lifecycle.shutdown().status, 'failed')
        self.assertFalse(self.observations)

    def test_uuid_mismatch_uses_context_handle_and_refuses_context_creation(self):
        self.lifecycle.qualify_single_visible_device()
        self.driver.device_uuid = bytes.fromhex('ffeeddccbbaa99887766554433221100')
        with self.assertRaises(LifecycleError): self.device()
        self.assertEqual(self.driver.uuid_query_handles, [self.driver.device_handle])
        self.assertNotIn('cuCtxCreate_v2', self.driver.calls)
        self.assertEqual(self.lifecycle.shutdown().status, 'failed')
        self.assertFalse(self.observations)


if __name__ == '__main__': unittest.main()
