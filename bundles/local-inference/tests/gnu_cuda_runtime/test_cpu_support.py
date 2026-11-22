"""Intercept pinned CPU staging import without loading any native library."""
import ctypes
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

sys.dont_write_bytecode=True
sys.path.insert(0,str(Path(__file__).resolve().parents[2]/'.ai/workers/local-inference/lib/local-tinygrad-gnu'))
import cpu_support

class CPUSupportTests(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.addCleanup(self.tmp.cleanup)
        self.root=Path(self.tmp.name)
        self.gnu=self.root/'gnu';self.tiny=self.root/'tiny'
        self.provider=self.gnu/'python/lib/libgcc_s.so.1'
        self.provider.parent.mkdir(parents=True);self.provider.write_bytes(b'never load')
        self.module=self.tiny/'tinygrad/runtime/ops_cpu.py'
        self.module.parent.mkdir(parents=True);self.module.write_text('# fixture')

    def test_only_selected_libgcc_load_reaches_injected_native(self):
        calls=[]
        def native(name,*args,**kwargs):
            calls.append(name);return SimpleNamespace(_name=name)
        def import_cpu(name):
            self.assertEqual(name,'tinygrad.runtime.ops_cpu')
            with self.assertRaises(RuntimeError):ctypes.CDLL('/ambient/libgcc_s.so.1')
            return SimpleNamespace(__file__=str(self.module),CPUProgram=SimpleNamespace(rt_lib=ctypes.CDLL('libgcc_s.so.1')))
        with patch.object(ctypes,'CDLL',native),patch.object(cpu_support.importlib,'import_module',side_effect=import_cpu):
            cpu_support.install_cpu_support(self.gnu,self.tiny)
            self.assertIs(ctypes.CDLL,native)
        self.assertEqual(calls,[str(self.provider)])

    def test_missing_selected_provider_refuses_before_import(self):
        self.provider.unlink()
        with patch.object(cpu_support.importlib,'import_module',side_effect=AssertionError('ambient import')):
            with self.assertRaises(FileNotFoundError):cpu_support.install_cpu_support(self.gnu,self.tiny)

    def test_swallowed_native_load_failure_still_refuses(self):
        module=SimpleNamespace(__file__=str(self.module),CPUProgram=SimpleNamespace(rt_lib=None))
        with patch.object(cpu_support.importlib,'import_module',return_value=module):
            with self.assertRaisesRegex(RuntimeError,'not loaded exactly'):cpu_support.install_cpu_support(self.gnu,self.tiny)

    def test_import_error_restores_loader_and_preloaded_module_refuses(self):
        native=ctypes.CDLL
        with patch.object(cpu_support.importlib,'import_module',side_effect=ValueError('partial import')):
            with self.assertRaises(ValueError):cpu_support.install_cpu_support(self.gnu,self.tiny)
        self.assertIs(ctypes.CDLL,native)
        with patch.dict(sys.modules,{'tinygrad.runtime.ops_cpu':object()}):
            with self.assertRaisesRegex(RuntimeError,'preceded'):cpu_support.install_cpu_support(self.gnu,self.tiny)

if __name__=='__main__':unittest.main()
