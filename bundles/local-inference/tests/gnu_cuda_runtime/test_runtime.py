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

SOURCE = Path(__file__).resolve().parents[2] / '.ai/workers/local-inference/lib/local-tinygrad-gnu'
sys.dont_write_bytecode = True
sys.path.insert(0, str(SOURCE))
import runtime


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
