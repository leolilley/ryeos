"""Close the pinned ops_cpu import's direct libgcc_s load for CPU weight staging."""
from __future__ import annotations
import ctypes
import importlib
import sys
from pathlib import Path
from bootstrap_support import _member


def install_cpu_support(gnu: Path, tinygrad: Path) -> None:
    if 'tinygrad.runtime.ops_cpu' in sys.modules:
        raise RuntimeError('CPU support preceded exact GNU binding')
    provider = _member(gnu, 'python/lib/libgcc_s.so.1')
    expected = _member(tinygrad, 'tinygrad/runtime/ops_cpu.py')
    native = ctypes.CDLL

    def selected(name, *args, **kwargs):
        if name != 'libgcc_s.so.1':
            raise RuntimeError('unselected direct CPU support library')
        return native(str(provider), *args, **kwargs)

    # The existing worker is single-threaded before session reader startup.
    # support.c's DLL class is already imported by the exact resolver; no
    # arbitrary module import is allowed during this finite interception.
    ctypes.CDLL = selected
    try:
        module = importlib.import_module('tinygrad.runtime.ops_cpu')
    finally:
        ctypes.CDLL = native
    if Path(module.__file__).resolve(strict=True) != expected:
        raise RuntimeError('CPU support source escaped exact tinygrad')
    handle = module.CPUProgram.rt_lib
    if handle is None or handle._name != str(provider):
        raise RuntimeError('CPU support libgcc provider was not loaded exactly')
