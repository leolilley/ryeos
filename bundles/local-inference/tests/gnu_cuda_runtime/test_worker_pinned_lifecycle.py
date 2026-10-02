"""Joint real worker/generator/tinygrad path with an injected driver only.

No weights/native libraries/CPU compiler children or numerical claims. Reuses
GPU-cleanup's committed test fixture from this checkout's complete closure.
"""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sys
from types import SimpleNamespace
import unittest
from unittest.mock import patch

ROOT=Path(__file__).resolve().parents[2]
SOURCE=ROOT/'.ai/workers/local-inference/lib/local-tinygrad-gnu'
sys.dont_write_bytecode=True

class WorkerPinnedLifecycleTests(unittest.TestCase):
    def test_actual_main_worker_qwen_generator_ptx_and_shutdown(self):
        fixture_path=ROOT/'tests/cuda_lifecycle/test_pinned_integration.py'
        spec=importlib.util.spec_from_file_location('selected_cleanup_fixture',fixture_path)
        fixture_module=importlib.util.module_from_spec(spec)
        spec.loader.exec_module(fixture_module)
        fixture=fixture_module.PinnedIntegrationTests('test_real_tensor_ptx_compile_realize_and_runtime_cache_path')
        fixture_module.PinnedIntegrationTests.setUpClass()
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        self.assertEqual(Path(sys.modules['cuda_lifecycle'].__file__).resolve(),SOURCE/'cuda_lifecycle/__init__.py')
        sys.path.insert(0,str(SOURCE))
        for name in ('model','model_contract','tokenizer','session'):
            sys.modules.pop(name,None)
        import model
        import session
        import runtime
        from tinygrad import Tensor
        with patch('ctypes.CDLL', side_effect=OSError('offline injected CPU import')):
            from tinygrad.runtime.ops_cpu import CPUDevice
        from tinygrad.device import Compiled, LRUAllocator
        # CPU backing for input bytes/from_blob; avoid its native service kernels.
        from tinygrad.runtime.ops_cpu import CPUAllocator
        def cpu_init(dev,name):
            allocator=CPUAllocator.__new__(CPUAllocator)
            LRUAllocator.__init__(allocator,dev)
            Compiled.__init__(dev,name,allocator,[],None)
            dev.ring_pos=0
            dev.synchronize=lambda:None
        with patch.object(CPUDevice,'__init__',cpu_init):
            qwen=model.QwenModel.__new__(model.QwenModel)
            qwen.contract=SimpleNamespace(model_id='qwen3-0.6b',context_ceiling=2048,output_ceiling=8)
            # Actual QwenModel.generate performs Tensor/UOp/realize/item. The
            # synthetic forward has no Transformer or model weights. Injected
            # driver launches intentionally do no arithmetic.
            qwen._fresh_model=lambda:lambda tokens,pos,temperature:(tokens.sum()*0).reshape(1)
            tokenizer=SimpleNamespace(encode=lambda _: [1,2],end_tokens=set(),
                stream_decoder=lambda:lambda token:'OK' if token is not None else '')
            request=json.loads((ROOT/'authoring/contracts/qwen3-0.6b-gnu-cuda-compatibility-request-v1.json').read_text())
            request['max_tokens']=1
            body=json.dumps(request,separators=(',',':'))
            outer={'request_body':body,'request_body_sha256':hashlib.sha256(body.encode()).hexdigest(),'requested_output_ceiling':8}
            frames=[{'kind':'request','request_id':'original','body':outer}]
            def read(_):
                if frames: return frames.pop(0)
                raise EOFError('original session owner closed request input')
            emitted=[]
            fixture.lifecycle._observe=lambda obs:emitted.append(('closed',obs))
            with patch.object(session,'QwenTokenizer',return_value=tokenizer),patch.object(session,'QwenModel',return_value=qwen),\
                 patch.object(session,'render_chat',return_value='synthetic complete prompt'),\
                 patch.object(session,'_read_frame',side_effect=read),\
                 patch.object(session,'_write_frame',side_effect=lambda fd,lock,kind,rid,body:emitted.append((kind,body))),\
                 patch.object(session.os,'fstat',return_value=SimpleNamespace(st_mode=0o140000)),\
                 patch.dict(os.environ,{'RYEOS_LOCAL_MODEL_PROFILE':'qwen3-0.6b','RYEOS_SESSION_FD':'3'}):
                self.assertEqual(runtime.run_with_lifecycle(fixture.lifecycle,lambda:session),0)
            self.assertEqual(emitted[0][0],'ready')
            self.assertEqual([kind for kind,_ in emitted].count('final'),1)
            final=next(body for kind,body in emitted if kind=='final')
            self.assertEqual(final['usage'],{'input_tokens':2,'output_tokens':1,'reasoning_tokens':None})
            self.assertEqual(emitted[-1][0],'closed')
            self.assertIn('cuLaunchKernel',fixture.driver.calls)
            self.assertIn('cuModuleLoadData',fixture.driver.calls)
            self.assertIn('cuMemFreeHost',fixture.driver.calls)
            self.assertEqual(fixture.lifecycle.inventory(),dict.fromkeys(['context','device','host','module','event'],0))
            count=len(fixture.driver.calls)
            with self.assertRaises(fixture_module.LifecycleError):fixture.cuda.cuCtxSynchronize()
            self.assertEqual(len(fixture.driver.calls),count)

if __name__=='__main__':unittest.main()
