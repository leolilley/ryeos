"""Actual runtime/session/private-channel fault joins with pinned injected CUDA.

The combined product checkout has runtime/session/cleanup_channel beside the
lifecycle module. During disjoint-source preparation an explicit
RYEOS_JOIN_RUNTIME_SOURCE may name GPU-runtime's private exact source checkout.
No native library, GPU, weights, compiler child, provider or daemon runs.
"""
from __future__ import annotations

import importlib.util
import json
import os
from pathlib import Path
import socket
import struct
import sys
from types import SimpleNamespace
import unittest
from unittest import mock

sys.dont_write_bytecode = True
SOURCE = Path(__file__).resolve().parents[2] / '.ai/workers/local-inference/lib/local-tinygrad-gnu'
RUNTIME_SOURCE = Path(os.environ.get('RYEOS_JOIN_RUNTIME_SOURCE', SOURCE)).resolve()
sys.path.insert(0, str(SOURCE))
import test_pinned_integration as pinned
from cuda_lifecycle import LifecycleError


def source_module(name: str, member: str):
    spec = importlib.util.spec_from_file_location(name, RUNTIME_SOURCE / member)
    if spec is None or spec.loader is None: raise AssertionError('complete runtime module unavailable')
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module  # dataclass resolution needs the selected module identity
    try: spec.loader.exec_module(module)
    except BaseException:
        sys.modules.pop(name, None)
        raise
    return module


class WorkerFaultJoinTests(unittest.TestCase):
    def setUp(self):
        required = ('runtime.py', 'session.py', 'cleanup_channel.py', 'bootstrap_support.py')
        missing = [member for member in required if not (RUNTIME_SOURCE / member).is_file()]
        if missing:
            self.fail(f'selected runtime source is incomplete at {RUNTIME_SOURCE}: {", ".join(missing)}')
        pinned.PinnedIntegrationTests.setUpClass()  # verify exact selected vendor source pin
        fixture = pinned.PinnedIntegrationTests('test_real_tensor_ptx_compile_realize_and_runtime_cache_path')
        fixture.require_single_visible_device = True
        fixture.setUp()
        self.fixture = fixture
        self.addCleanup(fixture.doCleanups)
        self.assertEqual(Path(sys.modules['cuda_lifecycle'].__file__).resolve(), SOURCE / 'cuda_lifecycle/__init__.py')
        self.path = mock.patch.object(sys, 'path', [str(RUNTIME_SOURCE), *sys.path])
        self.path.start(); self.addCleanup(self.path.stop)
        self.runtime = source_module('selected_join_runtime', 'runtime.py')
        self.wire = source_module('selected_join_cleanup_channel', 'cleanup_channel.py')
        with mock.patch.dict(sys.modules, {
            'model': SimpleNamespace(QwenModel=object),
            'tokenizer': SimpleNamespace(QwenTokenizer=object, render_chat=lambda *_: ''),
        }):
            self.session = source_module('selected_join_session', 'session.py')
        self.assertEqual(Path(self.runtime.__file__).resolve(), RUNTIME_SOURCE / 'runtime.py')
        self.assertEqual(Path(self.session.__file__).resolve(), RUNTIME_SOURCE / 'session.py')

        self.session_parent, session_child = socket.socketpair()
        self.addCleanup(self.session_parent.close)
        session_fd = session_child.detach()
        self.addCleanup(lambda: self.close_fd(session_fd))
        self.cleanup_parent, cleanup_child = socket.socketpair()
        self.addCleanup(self.cleanup_parent.close)
        cleanup_fd = cleanup_child.detach()
        self.addCleanup(lambda: self.close_fd(cleanup_fd))
        self.session_fd, self.cleanup_fd = session_fd, cleanup_fd
        self.bindings = {key: format(index, '064x') for index, key in enumerate(sorted(self.wire.BINDINGS), 1)}

    @staticmethod
    def close_fd(fd):
        try: os.close(fd)
        except OSError: pass

    def frame(self, kind, **changes):
        body = json.dumps(dict(self.bindings, version=1, kind=kind, **changes),
                          separators=(',', ':')).encode('utf-8')
        return struct.pack('>I', len(body)) + body

    def preflight_frame(self):
        device = dict(role='primary', destination='/dev/nvidia0', access='read_write', major=195, minor=0)
        bounds = dict(maximum_entries=4, maximum_depth=2,
                      maximum_file_bytes=4096, maximum_total_bytes=16384)
        value = dict(version=3, kind='preflight', bindings=self.bindings, challenge='8' * 64,
            inputs=dict(resource_selection=dict(stable_id='selected-gpu', **{'class': 'gpu'},
                matched_facts={}, observation_contract_digest='6' * 64, device_binding_digest='7' * 64,
                access='execution_restricted', enforcement='character_device_grant',
                character_devices=[device]),
                policy=dict(schema='ryeos.local_tinygrad_nvml_assignment_policy.v1',
                            primary_device_role='primary', l4_query_policy=None),
                handler_source_sha256='9' * 64, provider_manifest_sha256='a' * 64,
                gnu_bounds=bounds, driver_bounds=bounds))
        body = json.dumps(value, separators=(',', ':')).encode()
        return struct.pack('>I', len(body)) + body

    def object_frame(self, value):
        body = json.dumps(value, sort_keys=True, separators=(',', ':'), allow_nan=False).encode()
        return struct.pack('>I', len(body)) + body

    def channel(self, shutdown_frame):
        # The same inherited socket carries preflight, its candidate, matching
        # init-v2 and shutdown. This does not simulate D's held-process/finance.
        channel = self.wire.CleanupChannel.preflight(self.cleanup_fd, self.session_fd)
        self.cleanup_parent.sendall(self.preflight_frame())
        request = channel.read_preflight()
        inputs = request.values['inputs']
        primary = inputs['resource_selection']['character_devices'][0]
        candidate = dict(version=1, stable_resource_id='selected-gpu',
                         device_binding_digest='7' * 64, primary_device=primary,
                         uuid_v2_hex=self.fixture.expected_uuid_v2_hex, nvml_shutdown='closed')
        channel.report_preflight(candidate)
        result = self.wire.decode_preflight_result(self.wire._read_body(self.cleanup_parent.fileno()), request)
        init = dict(self.bindings, version=2, kind='init', uuid_v2_hex=result['uuid_v2_hex'],
                    gnu_bounds=inputs['gnu_bounds'], driver_bounds=inputs['driver_bounds'])
        self.assertFalse(self.fixture.driver.calls, 'CUDA calls preceded matching cleanup init')
        self.cleanup_parent.sendall(self.object_frame(init) + shutdown_frame)
        channel.accept_init()
        self.assertEqual(channel.uuid_v2_hex, self.fixture.expected_uuid_v2_hex,
                         'accepted D UUID comparison differs from lifecycle expectation')
        self.assertFalse(self.fixture.driver.calls, 'CUDA calls occurred before accepted cleanup init')
        self.fixture.lifecycle._observe = channel.observe
        self.fixture.lifecycle.qualify_single_visible_device()
        return channel

    def assert_no_retired(self):
        os.set_blocking(self.cleanup_parent.fileno(), False)
        with self.assertRaises(BlockingIOError): os.read(self.cleanup_parent.fileno(), 1)

    def test_partial_actual_cuda_device_in_worker_constructor_never_retires(self):
        channel = self.channel(self.frame('shutdown'))
        self.fixture.driver.fail = 'cuDeviceComputeCapability'
        def model(_root, _profile):
            self.fixture.device()  # actual pinned CUDADevice after native context acquisition
            self.fail('capability failure did not stop Worker construction')
        with mock.patch.object(self.session, 'QwenTokenizer', return_value=object()), \
             mock.patch.object(self.session, 'QwenModel', side_effect=model), \
             mock.patch.dict(os.environ, {'RYEOS_SESSION_FD': str(self.session_fd),
                                          'RYEOS_LOCAL_MODEL_PROFILE': 'qwen3-0.6b'}):
            with self.assertRaisesRegex(RuntimeError, 'CUDA normal shutdown evidence unavailable'):
                self.runtime.run_with_lifecycle(self.fixture.lifecycle, lambda: self.session,
                                                channel.authorize_shutdown)
        self.assertTrue(channel.authorized)
        self.assertFalse(channel.terminal)
        self.assertEqual(self.fixture.lifecycle.inventory()['context'], 1)
        self.assertEqual(self.fixture.lifecycle.shutdown().status, 'failed')
        self.assertIn('cuCtxCreate_v2', self.fixture.driver.calls)
        self.assertNotIn('cuCtxDestroy_v2', self.fixture.driver.calls)
        self.assert_no_retired()
        count = len(self.fixture.driver.calls)
        with self.assertRaises(LifecycleError): self.fixture.cuda.cuCtxSynchronize()
        self.assertEqual(len(self.fixture.driver.calls), count)

    def test_foreign_shutdown_after_live_worker_exit_frees_cuda_but_refuses_retired(self):
        channel = self.channel(self.frame('shutdown', process_owner='f' * 64))
        def model(_root, _profile):
            self.fixture.device()  # actual pinned selected context in Worker constructor
            return object()
        # A malformed next frame ends the live session while leaving its
        # duplex peer open to inspect the actual ready publication.
        self.assertEqual(os.write(self.session_parent.fileno(), b'\0\0\0\0'), 4)
        with mock.patch.object(self.session, 'QwenTokenizer', return_value=object()), \
             mock.patch.object(self.session, 'QwenModel', side_effect=model), \
             mock.patch.dict(os.environ, {'RYEOS_SESSION_FD': str(self.session_fd),
                                          'RYEOS_LOCAL_MODEL_PROFILE': 'qwen3-0.6b'}):
            with self.assertRaisesRegex(ValueError, 'original owner bindings changed'):
                self.runtime.run_with_lifecycle(self.fixture.lifecycle, lambda: self.session,
                                                channel.authorize_shutdown)
        self.assertFalse(channel.authorized)
        self.assertFalse(channel.terminal)
        self.assertEqual(self.fixture.lifecycle.inventory(), dict.fromkeys(
            ['context', 'device', 'host', 'module', 'event'], 0))
        self.assertEqual(self.fixture.lifecycle.shutdown().status, 'failed')
        self.assertIn('cuCtxSynchronize', self.fixture.driver.calls)
        self.assertIn('cuCtxDestroy_v2', self.fixture.driver.calls)
        self.assert_no_retired()
        os.set_blocking(self.session_parent.fileno(), False)
        self.assertIn(b'"kind":"ready"', os.read(self.session_parent.fileno(), 4096))
        count = len(self.fixture.driver.calls)
        with self.assertRaises(LifecycleError): self.fixture.cuda.cuCtxSynchronize()
        self.assertEqual(len(self.fixture.driver.calls), count)


if __name__ == '__main__': unittest.main()
