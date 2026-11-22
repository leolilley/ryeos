"""Bounded offline fixtures: no native libraries, CUDA, weights or providers."""
import contextlib
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import yaml

SOURCE = Path(__file__).resolve().parents[2] / '.ai/workers/local-inference/lib/local-tinygrad-gnu'
sys.dont_write_bytecode = True
sys.path.insert(0, str(SOURCE))
import runtime
import cuda_lifecycle


class RuntimeTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        for member in ('python-gnu/python/bin/python3.14', 'python-gnu/python/lib/libc.so.6',
                       'python-gnu/python/lib/ld-linux-x86-64.so.2', 'python-gnu/python/lib/libgcc_s.so.1', 'runtime/lib/ld-musl-x86_64.so.1',
                       'toolchain/bin/clang', 'toolchain/bin/ld.lld',
                       'toolchain/lib/clang/20/lib/x86_64-alpine-linux-musl/libclang_rt.builtins-x86_64.a',
                       'tinygrad/tinygrad/runtime/support/c.py', 'driver/lib/libcuda.so.1'):
            p = self.root / member
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_bytes(b'fixture only; never load')
        for directory in ('python-gnu/python/lib/python3.14/lib-dynload', 'model', 'worker', 'scratch/home', 'scratch/cache', 'scratch/tmp'):
            (self.root / directory).mkdir(parents=True)
        self.layout = runtime.validate_paths(self.root, self.root / 'worker')
        self.env = dict(DEV='CUDA:PTX', JIT='0', PROFILE='0', RYEOS_LOCAL_MODEL_PROFILE='qwen3-0.6b', RYEOS_SESSION_FD='3')

    def test_environment_closes_ambient_controls_and_preserves_gnu(self):
        env = runtime.closed_environment(self.layout, dict(self.env, LD_PRELOAD='/host/evil', PYTHONPATH='/host', REGEN='1'))
        self.assertEqual(env['PYTHONHOME'], str(self.root / 'python-gnu/python'))
        self.assertEqual(env['PATH'], '')
        self.assertEqual((env['JIT'], env['PROFILE']), ('0', '0'))
        for key in ('LD_PRELOAD', 'PYTHONPATH', 'REGEN'):
            self.assertNotIn(key, env)

    def test_bad_channel_backend_and_profile_refuse(self):
        for key, value in [('DEV','CPU'), ('JIT','1'), ('PROFILE','1'), ('RYEOS_LOCAL_MODEL_PROFILE','qwen3-4b'), ('RYEOS_SESSION_FD','01'), ('RYEOS_SESSION_FD','2'), ('RYEOS_SESSION_FD','2147483648')]:
            with self.subTest(key=key, value=value), self.assertRaises((RuntimeError, ValueError)):
                runtime.closed_environment(self.layout, dict(self.env, **{key:value}))

    def test_each_missing_selected_component_refuses(self):
        for member in ('python-gnu/python/lib/libc.so.6', 'driver/lib/libcuda.so.1', 'toolchain/bin/clang', 'python-gnu/python/lib/libgcc_s.so.1'):
            p = self.root / member
            data = p.read_bytes(); p.unlink()
            with self.assertRaises(FileNotFoundError):
                runtime.validate_paths(self.root, self.root / 'worker')
            p.write_bytes(data)

    def test_driver_symlink_to_ambient_refuses(self):
        p = self.root / 'driver/lib/libcuda.so.1'; p.unlink()
        p.symlink_to(self.root / 'python-gnu/python/lib/libc.so.6')
        with self.assertRaisesRegex(ValueError, 'escaped'):
            runtime.validate_paths(self.root, self.root / 'worker')

    def test_missing_observer_refuses_before_prepare_or_cuda_import(self):
        with patch.object(runtime, 'prepare_environment', side_effect=AssertionError('too late')):
            with self.assertRaisesRegex(RuntimeError, 'binding'):
                runtime.run_product()

    def test_retained_tree_bounds_are_required_before_path_scan(self):
        with patch.object(runtime, 'validate_paths', side_effect=AssertionError('path scan ran')):
            for kwargs in ({}, {'gnu_bounds': {}}, {'driver_bounds': {}}):
                with self.subTest(kwargs=kwargs), self.assertRaisesRegex(ValueError, 'bounds'):
                    runtime.prepare_environment(**kwargs)

    def test_malformed_and_over_ceiling_bounds_refuse_before_path_scan(self):
        valid = dict(maximum_entries=100, maximum_depth=10,
                     maximum_file_bytes=1024, maximum_total_bytes=4096)
        with patch.object(runtime, 'validate_paths', side_effect=AssertionError('path scan ran')):
            with self.assertRaisesRegex(ValueError, 'exact admitted tree bounds'):
                runtime.prepare_environment(gnu_bounds=dict(valid, unexpected=1), driver_bounds=valid)
            over = dict(valid, maximum_entries=runtime.GNU_RUNTIME_BOUNDS_CEILING.maximum_entries + 1)
            with self.assertRaisesRegex(ValueError, 'relationship ceiling'):
                runtime.prepare_environment(gnu_bounds=over, driver_bounds=valid)

    def test_gnu_ceiling_matches_authored_unsigned_product_relationship(self):
        contract = yaml.safe_load(
            (SOURCE.parents[4] / 'authoring/drafts/config/ryeos-runtime/gnu-cuda-products-draft.yaml').read_text()
        )
        relationship = next(
            item for item in contract['product_relationships']['relationships']
            if item['name'] == 'gnu_runtime_to_private_worker'
        )
        authored = runtime.TreeBounds.from_contract(relationship['required_product']['bounds'])
        self.assertEqual(runtime.GNU_RUNTIME_BOUNDS_CEILING, authored)

    def test_channel_entry_requires_typed_v2_selection_before_prepare(self):
        channel = SimpleNamespace(
            gnu_bounds=None, driver_bounds=None, uuid_v2_hex=None,
            observe=lambda _: None, authorize_shutdown=lambda: None,
        )
        fake_channel = SimpleNamespace(
            CleanupChannel=lambda *args: channel,
            descriptor=lambda value: 7,
        )
        env = {'RYEOS_RESOURCE_CLEANUP_FD': '7', 'RYEOS_SESSION_FD': '3'}
        with patch.dict(sys.modules, {'cleanup_channel': fake_channel}), patch.dict(os.environ, env):
            with patch.object(runtime, 'prepare_environment', side_effect=AssertionError('provider preparation ran')):
                with self.assertRaisesRegex(ValueError, 'bounds'):
                    runtime.run_channel_product()

    def test_channel_entry_dispatches_only_typed_init_v2_comparisons(self):
        selected = runtime.TreeBounds(100, 10, 1024, 4096)
        channel = SimpleNamespace(
            gnu_bounds=selected, driver_bounds=selected, uuid_v2_hex='1' * 32,
            observe=lambda _: None, authorize_shutdown=lambda: None,
        )
        fake_channel = SimpleNamespace(CleanupChannel=lambda *args: channel, descriptor=lambda value: 7)
        env = {'RYEOS_RESOURCE_CLEANUP_FD': '7', 'RYEOS_SESSION_FD': '3'}
        with patch.dict(sys.modules, {'cleanup_channel': fake_channel}), patch.dict(os.environ, env):
            with patch.object(runtime, 'run_product', return_value='accepted') as run:
                self.assertEqual(runtime.run_channel_product(), 'accepted')
        self.assertEqual(run.call_args.kwargs['gnu_bounds'], selected)
        self.assertEqual(run.call_args.kwargs['driver_bounds'], selected)
        self.assertEqual(run.call_args.kwargs['expected_uuid_v2_hex'], '1' * 32)

    def test_product_requires_bounds_before_prepare_and_passes_typed_values(self):
        selected = runtime.TreeBounds(100, 10, 1024, 4096)
        with patch.object(runtime, 'prepare_environment', side_effect=AssertionError('prepare ran')):
            with self.assertRaisesRegex(ValueError, 'bounds'):
                runtime.run_product(observe=lambda _: None)
            with self.assertRaisesRegex(ValueError, 'UUID-v2'):
                runtime.run_product(
                    observe=lambda _: None, gnu_bounds=selected, driver_bounds=selected,
                )
            with self.assertRaisesRegex(ValueError, 'UUID-v2'):
                runtime.run_product(
                    observe=lambda _: None, gnu_bounds=selected, driver_bounds=selected,
                    expected_uuid_v2_hex='0' * 32,
                )
        calls = []
        generated = SimpleNamespace(
            __file__=str(self.root / 'tinygrad/tinygrad/runtime/autogen/cuda.py'),
            cuInit=lambda *_: (_ for _ in ()).throw(AssertionError('raw CUDA call before lifecycle')),
            cuDeviceGetCount=lambda *_: (_ for _ in ()).throw(AssertionError('raw count before lifecycle')),
        )
        lifecycle = SimpleNamespace(
            submission=contextlib.nullcontext,
            qualify_single_visible_device=lambda: calls.append(('qualify', None)),
            shutdown=lambda: calls.append(('shutdown', None)) or SimpleNamespace(
                status='closed', observation_delivered=True,
            ),
        )
        def install(**kwargs):
            calls.append(('install', kwargs))
            kwargs['publish_owner'](lifecycle)
            return lifecycle
        lifecycle_module = SimpleNamespace(
            install=install,
            _fence_installation_owners=cuda_lifecycle._fence_installation_owners,
            _InstallationFailureEvidence=cuda_lifecycle._InstallationFailureEvidence,
        )
        product_modules = {
            'compiler': SimpleNamespace(install_admitted_compiler=lambda _: calls.append(('compiler', None))),
            'cpu_support': SimpleNamespace(install_cpu_support=lambda *_: calls.append(('cpu', None))),
            'session': SimpleNamespace(main=lambda _: calls.append(('main', None)) or 0),
        }
        with patch.object(runtime, 'prepare_environment', return_value=(self.root, self.root, self.root, self.root)) as prepare:
            with patch.object(runtime, 'verify_origins'):
                with patch.dict(sys.modules, {
                    'cuda_lifecycle': lifecycle_module,
                    'tinygrad.runtime.autogen.cuda': generated,
                    **product_modules,
                }):
                    result = runtime.run_product(
                        observe=lambda _: None, gnu_bounds=selected, driver_bounds=selected,
                        expected_uuid_v2_hex='1' * 32,
                    )
        self.assertIs(prepare.call_args.kwargs['gnu_bounds'], selected)
        self.assertIs(prepare.call_args.kwargs['driver_bounds'], selected)
        self.assertEqual(calls[0][1]['expected_uuid_v2_hex'], '1' * 32)
        self.assertEqual(calls[0][1]['bindings'], generated)
        self.assertIs(calls[0][1]['require_single_visible_device'], True)
        self.assertEqual(
            [name for name, _ in calls],
            ['install', 'qualify', 'compiler', 'cpu', 'main', 'shutdown'],
        )
        self.assertEqual(result, 0)

    def test_product_never_calls_device_count_before_lifecycle_install(self):
        selected = runtime.TreeBounds(100, 10, 1024, 4096)
        calls = []
        generated = SimpleNamespace(
            __file__=str(self.root / 'tinygrad/runtime/autogen/cuda.py'),
            cuInit=lambda *_: (_ for _ in ()).throw(AssertionError('raw CUDA init')),
            cuDeviceGetCount=lambda *_: (_ for _ in ()).throw(AssertionError('raw device count')),
        )
        lifecycle = SimpleNamespace()
        def install(**kwargs):
            calls.append(('install', kwargs))
            kwargs['publish_owner'](lifecycle)
            return lifecycle
        lifecycle_module = SimpleNamespace(
            install=install,
            _fence_installation_owners=cuda_lifecycle._fence_installation_owners,
            _InstallationFailureEvidence=cuda_lifecycle._InstallationFailureEvidence,
        )
        with patch.object(runtime, 'prepare_environment', return_value=(self.root, self.root, self.root, self.root)):
            with patch.object(runtime, 'verify_origins'):
                with patch.dict(sys.modules, {
                    'cuda_lifecycle': lifecycle_module,
                    'tinygrad.runtime.autogen.cuda': generated,
                }):
                    with patch.object(runtime, 'run_with_lifecycle', side_effect=RuntimeError('stop after install')):
                        with self.assertRaisesRegex(RuntimeError, 'stop after install'):
                            runtime.run_product(
                                observe=lambda _: None, gnu_bounds=selected, driver_bounds=selected,
                                expected_uuid_v2_hex='1' * 32,
                            )
        self.assertEqual([name for name, _ in calls], ['install'])
        self.assertIs(calls[0][1]['require_single_visible_device'], True)

    def failed_install_join(self, install):
        selected = runtime.TreeBounds(100, 10, 1024, 4096)
        with patch.object(runtime, 'prepare_environment', return_value=(self.root,) * 4), \
                patch.object(runtime, 'verify_origins'), \
                patch.object(cuda_lifecycle, 'install', side_effect=install), \
                patch.object(runtime, 'run_with_lifecycle') as enter_session:
            try:
                runtime.run_product(
                    observe=lambda _: None, bindings=object(),
                    gnu_bounds=selected, driver_bounds=selected,
                    expected_uuid_v2_hex='1' * 32,
                )
            except BaseException as error:
                traceback = error.__traceback__
                while traceback and traceback.tb_frame.f_code.co_name != 'run_product':
                    traceback = traceback.tb_next
                self.assertIsNotNone(traceback)
                evidence = traceback.tb_frame.f_locals['failure_evidence']
                enter_session.assert_not_called()
                return error, evidence
        self.fail('invalid installation entered the session')

    def test_missing_and_substituted_returned_owner_are_fenced_before_session(self):
        for mode in ('missing', 'substituted'):
            calls = []
            first = SimpleNamespace(abort_installation=lambda error: calls.append(('first', error)))
            returned = SimpleNamespace(abort_installation=lambda error: calls.append(('returned', error)))
            def install(**kwargs):
                if mode == 'substituted':
                    kwargs['publish_owner'](first)
                return returned
            with self.subTest(mode=mode):
                error, evidence = self.failed_install_join(install)
                self.assertIsInstance(error, RuntimeError)
                self.assertIn('unpublished owner', str(error))
                self.assertEqual([name for name, _ in calls],
                                 ['returned'] if mode == 'missing' else ['first', 'returned'])
                self.assertTrue(all(primary is error for _, primary in calls))
                self.assertIs(evidence.owners[-1], returned)
                self.assertFalse(evidence.fence_failures)

    def test_duplicate_publication_retains_first_and_attempts_distinct_incoming(self):
        for distinct in (False, True):
            calls = []
            first = SimpleNamespace(abort_installation=lambda error: calls.append(('first', error)))
            incoming = SimpleNamespace(abort_installation=lambda error: calls.append(('incoming', error))) if distinct else first
            def install(**kwargs):
                kwargs['publish_owner'](first)
                kwargs['publish_owner'](incoming)
                self.fail('duplicate publication was accepted')
            with self.subTest(distinct=distinct):
                error, evidence = self.failed_install_join(install)
                self.assertIn('duplicated', str(error))
                self.assertIs(evidence.owners[0], first)
                self.assertIs(evidence.owners[1], incoming)
                self.assertEqual([name for name, _ in calls], ['first', 'incoming'] if distinct else ['first'])
                self.assertTrue(all(primary is error for _, primary in calls))

    def test_runtime_preserves_primary_and_second_owner_after_first_fence_failure(self):
        primary = RuntimeError('installation failed after two publications')
        secondary = ValueError('first owner fence failed')
        calls = []
        def first_fence(error):
            calls.append(('first', error))
            raise secondary
        first = SimpleNamespace(abort_installation=first_fence)
        second = SimpleNamespace(abort_installation=lambda error: calls.append(('second', error)))
        def install(**kwargs):
            kwargs['publish_owner'](first)
            # The duplicate callback error is the initiating production failure.
            try:
                kwargs['publish_owner'](second)
            except RuntimeError:
                raise primary
        error, evidence = self.failed_install_join(install)
        self.assertIs(error, primary)
        self.assertEqual([name for name, _ in calls], ['first', 'second'])
        self.assertTrue(all(observed is primary for _, observed in calls))
        self.assertIs(evidence.fence_failures[0], secondary)

    def test_runtime_helper_error_is_secondary_and_primary_traceback_retains_custody(self):
        primary = RuntimeError('install failure')
        helper_error = ValueError('evidence helper failure')
        calls = []
        owner = SimpleNamespace(abort_installation=lambda error: calls.append(error))
        def install(**kwargs):
            kwargs['publish_owner'](owner)
            raise primary
        actual_fence = cuda_lifecycle._fence_installation_owners
        def fence(error, evidence):
            actual_fence(error, evidence)
            raise helper_error
        with patch.object(cuda_lifecycle, '_fence_installation_owners', side_effect=fence):
            error, evidence = self.failed_install_join(install)
        self.assertIs(error, primary)
        self.assertEqual(calls, [primary])
        self.assertIs(evidence.owners[0], owner)
        self.assertIs(evidence.evidence_failures[0], helper_error)

    def test_runtime_hostile_metadata_preserves_primary_and_all_owner_attempts(self):
        class HostileError(RuntimeError):
            def __getattribute__(self, name):
                if name.startswith('_installation_') or name == '__notes__':
                    raise ValueError('metadata read refused')
                return super().__getattribute__(name)
            def __setattr__(self, name, value):
                if name.startswith('_installation_') or name == '__notes__':
                    raise ValueError('metadata write refused')
                return super().__setattr__(name, value)
        primary = HostileError('original install error')
        secondary = ValueError('first fence failure')
        calls = []
        def first_fence(error):
            calls.append(('first', error))
            raise secondary
        first = SimpleNamespace(abort_installation=first_fence)
        second = SimpleNamespace(abort_installation=lambda error: calls.append(('second', error)))
        def install(**kwargs):
            kwargs['publish_owner'](first)
            try:
                kwargs['publish_owner'](second)
            except RuntimeError:
                raise primary
        error, evidence = self.failed_install_join(install)
        self.assertIs(error, primary)
        self.assertEqual([name for name, _ in calls], ['first', 'second'])
        self.assertTrue(all(observed is primary for _, observed in calls))
        self.assertIs(evidence.owners[0], first)
        self.assertIs(evidence.owners[1], second)
        self.assertIs(evidence.fence_failures[0], secondary)
        self.assertTrue(evidence.evidence_failures)

    def test_lifecycle_wraps_import_session_and_constructor_failure(self):
        for fail in ('import', 'constructor', None):
            calls=[]
            @contextlib.contextmanager
            def submission():
                calls.append('begin')
                try: yield
                finally: calls.append('end')
            def load():
                calls.append('import')
                if fail=='import': raise ValueError('import failure')
                def main(life):
                    with life.submission():
                        calls.append('construct')
                        if fail=='constructor': raise ValueError('constructor failure')
                    with life.submission(): calls.append('execute')
                    return 0
                return SimpleNamespace(main=main)
            def shutdown():
                calls.append('shutdown')
                return SimpleNamespace(status='closed', observation_delivered=True)
            life=SimpleNamespace(submission=submission, shutdown=shutdown)
            if fail:
                with self.assertRaises(ValueError): runtime.run_with_lifecycle(life,load)
            else: self.assertEqual(runtime.run_with_lifecycle(life,load),0)
            self.assertEqual(calls[-1], 'shutdown')
            self.assertEqual(calls[:2], ['begin','import'])

    def test_busy_failed_and_unobserved_shutdown_remain_uncertain(self):
        for status, delivered in [('busy',False), ('failed',False), ('closed',False)]:
            life=SimpleNamespace(submission=contextlib.nullcontext,
                shutdown=lambda: SimpleNamespace(status=status, observation_delivered=delivered))
            with self.assertRaisesRegex(RuntimeError,'uncertain'):
                runtime.run_with_lifecycle(life, lambda: SimpleNamespace(main=lambda _:0))

    def test_publication_eof_remains_error_after_valid_retirement(self):
        class InputClosed(EOFError): pass
        life=SimpleNamespace(submission=contextlib.nullcontext,
            shutdown=lambda:SimpleNamespace(status='closed',observation_delivered=True))
        def main(_): raise EOFError('partial publication')
        session=SimpleNamespace(main=main,SessionInputClosed=InputClosed)
        with self.assertRaisesRegex(EOFError,'publication'):
            runtime.run_with_lifecycle(life,lambda:session)


class RequestTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        spec=importlib.util.spec_from_file_location('gpu_session_fixture', SOURCE/'session.py')
        cls.session=importlib.util.module_from_spec(spec)
        # Only model/tokenizer collaborators are injected; test actual Worker path.
        with patch.dict(sys.modules, {'model':SimpleNamespace(QwenModel=object),
                                     'tokenizer':SimpleNamespace(QwenTokenizer=object,render_chat=lambda *args:'packed')}):
            spec.loader.exec_module(cls.session)
        cls.request=json.loads((SOURCE.parents[4]/'authoring/contracts/qwen3-0.6b-gnu-cuda-compatibility-request-v1.json').read_text())

    def worker(self,count):
        w=self.session.Worker(load_model=False)
        w.model=SimpleNamespace(model_id='qwen3-0.6b',output_ceiling=256,context_ceiling=2048,
            generate=lambda *args:iter([7]))
        w.tokenizer=SimpleNamespace(encode=lambda _:list(range(count)),end_tokens=set(),
            stream_decoder=lambda:lambda token:'OK' if token is not None else '')
        return w

    def execute(self,w,request):
        import threading
        body=json.dumps(request,separators=(',',':'))
        outer={'request_body':body,'request_body_sha256':hashlib.sha256(body.encode()).hexdigest(),'requested_output_ceiling':8}
        with patch.dict(os.environ, {'RYEOS_LOCAL_MODEL_PROFILE':'qwen3-0.6b'}):
            return w.execute('one',outer,threading.Event(),lambda _:None)

    def test_real_worker_reports_packing_and_rejects_257_without_truncation(self):
        with patch.dict(os.environ, {'RYEOS_LOCAL_MODEL_PROFILE':'qwen3-0.6b'}):
            w=self.worker(256); result=self.execute(w,self.request)
            self.assertEqual(result['usage']['input_tokens'],256)
            w=self.worker(257)
            w.model.generate=lambda *args: (_ for _ in ()).throw(AssertionError('backend touched'))
            with self.assertRaises(ValueError): self.execute(w,self.request)

    def test_policy_rejects_sampling_thinking_tools_before_generation(self):
        with patch.dict(os.environ, {'RYEOS_LOCAL_MODEL_PROFILE':'qwen3-0.6b'}):
            w=self.worker(1)
            w.model.generate=lambda *args: (_ for _ in ()).throw(AssertionError('backend touched'))
            for key,value in [('temperature',1),('seed',1),('enable_thinking',True),('tools',[{}]),('max_tokens',9)]:
                with self.subTest(key=key), self.assertRaises(ValueError):
                    self.execute(w,dict(self.request,**{key:value}))

if __name__=='__main__': unittest.main()
