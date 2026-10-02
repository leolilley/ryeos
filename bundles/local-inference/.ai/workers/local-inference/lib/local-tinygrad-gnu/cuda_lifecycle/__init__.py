"""Cooperative normal close for the pinned, trusted single-device CUDA:PTX product.

No transport, process, deadline or authentication authority lives here. Install
before importing CUDA consumers; the runtime owns the exact binding resolver.
"""
from __future__ import annotations

import ctypes
import functools
import importlib
import sys
import threading
from contextlib import contextmanager
from dataclasses import dataclass
from typing import Callable


class LifecycleError(RuntimeError):
    pass


@dataclass(frozen=True)
class ShutdownResult:
    status: str  # busy, failed, closed
    observation_delivered: bool = False
    error: str | None = None


@dataclass(frozen=True)
class CloseObservation:
    """Product semantics only. D binds/encodes these through its private owner."""
    selected_device: int
    contexts_destroyed: int
    device_allocations_freed: int
    host_allocations_freed: int
    modules_unloaded: int
    events_destroyed: int


# Exact entry points reached by the pinned graph-free producer. Unknown native
# functions fail closed, including raw symbol lookup and context-independent APIs.
_ACQUIRE = {
    "cuCtxCreate_v2": ("context", "CUcontext"),
    "cuModuleLoadData": ("module", "CUmodule"),
    "cuMemAlloc_v2": ("device", "CUdeviceptr"),
    "cuMemHostAlloc": ("host", "c_void_p"),
    "cuEventCreate": ("event", "CUevent"),
}
_RELEASE = {
    "cuCtxDestroy_v2": "context", "cuModuleUnload": "module",
    "cuMemFree_v2": "device", "cuMemFreeHost": "host",
    "cuEventDestroy_v2": "event",
}
_ALLOW = frozenset(_ACQUIRE) | frozenset(_RELEASE) | {
    "cuInit", "cuDeviceGet", "cuDeviceComputeCapability", "cuCtxSetCurrent",
    "cuCtxSynchronize", "cuModuleGetFunction", "cuFuncSetAttribute",
    "cuLaunchKernel", "cuMemcpyHtoDAsync_v2", "cuMemcpyDtoH_v2",
    "cuEventRecord", "cuEventSynchronize", "cuEventElapsedTime",
}


def _value(handle):
    if isinstance(handle, int): return handle
    if isinstance(handle, (ctypes.c_void_p, ctypes.c_uint64, ctypes.c_int)):
        return handle.value or 0
    return ctypes.cast(handle, ctypes.c_void_p).value or 0


@dataclass
class _Resource:
    kind: str
    handle: object
    size: int = 0


class Lifecycle:
    def __init__(self, bindings, selected_device: int, observe: Callable | None):
        self.selected_device, self._observe = selected_device, observe
        self._bindings = bindings
        self._lock = threading.RLock()  # in-memory serialization, no thread owner
        self._local = threading.local()
        self._active = 0
        self._state = "open"
        self._poison: str | None = None
        self._resources: dict[tuple[str, int], _Resource] = {}
        self._functions: dict[int, int] = {}
        self._counts = dict.fromkeys(("context", "device", "host", "module", "event"), 0)
        self._context_created = False
        self._device_handle: int | None = None
        self._hooks: list[tuple[object, str, object]] = []
        self._native = {name: getattr(bindings, name) for name in _ALLOW}
        self._result: ShutdownResult | None = None
        self._policy = lambda: None
        self._terminalize = lambda: None

    @contextmanager
    def submission(self):
        """Enclose construction or the entire request, including result copies.

        Closing rejects new outer submissions; an already admitted request may
        finish nested work. shutdown never waits for it or creates a deadline.
        """
        with self._lock:
            depth = getattr(self._local, "depth", 0)
            if self._poison:
                raise LifecycleError("CUDA inventory is uncertain")
            self._policy()
            if not depth:
                if self._state != "open" or self._poison:
                    raise LifecycleError("CUDA submissions are closed")
                if self._active:
                    raise LifecycleError("selected product permits one active submission")
                self._active += 1
            self._local.depth = depth + 1
        try:
            yield
        finally:
            with self._lock:
                self._local.depth -= 1
                if not self._local.depth: self._active -= 1

    def inventory(self) -> dict[str, int]:
        with self._lock:
            return {kind: sum(r.kind == kind for r in self._resources.values()) for kind in self._counts}

    def _fail(self, message):
        self._poison = self._poison or message
        raise LifecycleError(message)

    def _owned(self, kind, handle):
        if (kind, _value(handle)) not in self._resources:
            self._fail(f"unowned {kind} handle")

    def _range(self, kind, handle, size):
        ptr = _value(handle)
        if not any(r.kind == kind and _value(r.handle) <= ptr and
                   ptr + size <= _value(r.handle) + r.size for r in self._resources.values()):
            self._fail(f"unowned {kind} range")

    def _validate_call(self, name, args):
        if name == "cuInit" and args[0] != 0: self._fail("unselected driver initialization")
        if name == "cuDeviceGet" and args[1] != self.selected_device: self._fail("extra device")
        if name == "cuDeviceComputeCapability":
            if self._device_handle is None or _value(args[2]) != self.selected_device:
                self._fail("extra device")
        if name == "cuCtxCreate_v2":
            if self._context_created or args[1] != 0 or _value(args[2]) != self._device_handle:
                self._fail("extra or shared context")
        elif name == "cuCtxSetCurrent": self._owned("context", args[0])
        elif name not in {"cuInit", "cuDeviceGet", "cuDeviceComputeCapability"}:
            if len([r for r in self._resources.values() if r.kind == "context"]) != 1:
                self._fail("operation without owned context")
        if name == "cuMemHostAlloc" and args[2] != 1: self._fail("unselected host allocator")
        if name in {"cuMemHostAlloc", "cuMemAlloc_v2"} and args[1] <= 0: self._fail("invalid allocation")
        if name in _RELEASE: self._owned(_RELEASE[name], args[0])
        if name == "cuModuleGetFunction": self._owned("module", args[1])
        if name in {"cuLaunchKernel", "cuFuncSetAttribute"}:
            module = self._functions.get(_value(args[0]))
            if module is None or ("module", module) not in self._resources: self._fail("unowned function")
        if name == "cuLaunchKernel" and _value(args[8]) != 0: self._fail("nondefault stream")
        if name == "cuMemcpyHtoDAsync_v2":
            self._range("device", args[0], args[2]); self._range("host", args[1], args[2])
            if _value(args[3]): self._fail("nondefault stream")
        if name == "cuMemcpyDtoH_v2": self._range("device", args[1], args[2])
        if name in {"cuEventRecord", "cuEventSynchronize"}: self._owned("event", args[0])
        if name == "cuEventRecord" and _value(args[1]): self._fail("nondefault stream")
        if name == "cuEventCreate" and args[1] != 0: self._fail("unselected event flags")
        if name == "cuEventElapsedTime":
            self._owned("event", args[1]); self._owned("event", args[2])

    def _call(self, name, args):
        self._validate_call(name, args)
        # Pinned CUDADevice supplies its ordinal integer here, whereas the
        # generated binding consumes CUdevice. Forward the actual handle returned
        # by the selected cuDeviceGet instead of assuming handle == ordinal.
        native_args = (*args[:2], self._bindings.CUdevice(self._device_handle)) if name == "cuDeviceComputeCapability" else args
        try:
            status = self._native[name](*native_args)
        except BaseException:
            self._poison = self._poison or f"uncertain native {name}"
            raise
        # Track the output of a successful native acquisition BEFORE returning
        # to init_c_var, a constructor, cache insertion or any Python hook.
        if name in _ACQUIRE and status == 0:
            kind, typename = _ACQUIRE[name]
            typ = ctypes.c_void_p if typename == "c_void_p" else getattr(self._bindings, typename)
            handle = ctypes.cast(args[0], ctypes.POINTER(typ))[0]
            # Make an independent handle so caller mutation cannot erase ownership.
            handle = typ(_value(handle)) if typ in (ctypes.c_void_p, ctypes.c_uint64) else ctypes.cast(ctypes.c_void_p(_value(handle)), typ)
            key = kind, _value(handle)
            if not key[1] or key in self._resources: self._fail("ambiguous acquisition output")
            self._resources[key] = _Resource(kind, handle, args[1] if kind in {"host", "device"} else 0)
            if kind == "context": self._context_created = True
        if status != 0: self._fail(f"native {name} failed ({status})")
        if name == "cuDeviceGet":
            self._device_handle = ctypes.cast(args[0], ctypes.POINTER(self._bindings.CUdevice))[0]
        if name in _RELEASE:
            kind = _RELEASE[name]
            key = kind, _value(args[0])
            del self._resources[key]
            self._counts[kind] += 1
            if kind == "module": self._functions = {f: m for f, m in self._functions.items() if m != key[1]}
        if name == "cuModuleGetFunction":
            fn = ctypes.cast(args[0], ctypes.POINTER(self._bindings.CUfunction))[0]
            self._functions[_value(fn)] = _value(args[1])
        return status

    def _guarded(self, name):
        def call(*args):
            with self.submission(), self._lock:
                if name not in _ALLOW: self._fail(f"native {name} outside selected product")
                try:
                    return self._call(name, args)
                except BaseException:
                    self._poison = self._poison or f"uncertain native operation {name}"
                    raise
        return call

    def _hook(self, owner, name, function):
        setattr(owner, name, function)
        self._hooks.append((owner, name, function))

    def _validate_inventory(self):
        if any(getattr(owner, name) is not function for owner, name, function in self._hooks):
            self._fail("product hook replaced")
        self._policy()
        if self._poison: raise LifecycleError(self._poison)
        if not self._context_created or self.inventory()["context"] != 1:
            self._fail("missing complete selected context")

    def shutdown(self) -> ShutdownResult:
        """Close admission immediately. The existing owner quiesces busy work.

        A duplicate terminal call returns the SAME result without driver calls or
        callback replay. Failed/partial close never becomes a successful receipt.
        """
        with self._lock:
            if self._result is not None: return self._result
            self._state = "closing"
            if self._active: return ShutdownResult("busy", error="quiescence required")
            try:
                self._validate_inventory()
                context = next(r.handle for r in self._resources.values() if r.kind == "context")
                self._call("cuCtxSetCurrent", (context,))
                self._call("cuCtxSynchronize", ())
                for kind, name in (("event", "cuEventDestroy_v2"), ("module", "cuModuleUnload"),
                                   ("device", "cuMemFree_v2"), ("host", "cuMemFreeHost"),
                                   ("context", "cuCtxDestroy_v2")):
                    for resource in list(self._resources.values()):
                        if resource.kind == kind: self._call(name, (resource.handle,))
                if self._resources: self._fail("resources remain after close")
                self._state = "closed"  # callback cannot reenter any native API
                self._terminalize()
                observation = CloseObservation(self.selected_device, self._counts["context"], self._counts["device"],
                                               self._counts["host"], self._counts["module"], self._counts["event"])
                # Set terminal result BEFORE delivering: callback recursion cannot
                # initiate a second teardown. Failure is retained without replay.
                self._result = ShutdownResult("closed")
                if self._observe is not None:
                    self._observe(observation)
                    self._result = ShutdownResult("closed", observation_delivered=True)
            except BaseException as error:
                self._state = "failed"
                self._terminalize()
                self._result = ShutdownResult("failed", error=type(error).__name__ + ": " + str(error)[:160])
            return self._result


def install(*, bindings=None, selected_device: int = 0, observe: Callable | None = None) -> Lifecycle:
    """Install once after the exact DLL resolver, before any CUDA consumer.

    observe is optional for offline/source preparation ONLY. Production runtime
    must require D's admitted channel owner before importing/loading CUDA.
    """
    if isinstance(selected_device, bool) or selected_device != 0:
        raise LifecycleError("first product selects only device ordinal zero")
    if observe is not None and not callable(observe): raise LifecycleError("invalid observer")
    if any(name in sys.modules for name in ("tinygrad.runtime.ops_cuda", "tinygrad.runtime.graph.cuda")):
        raise LifecycleError("CUDA consumers preceded lifecycle installation")
    from tinygrad import helpers
    from tinygrad.device import Buffer, Device
    if helpers.JIT.value != 0 or helpers.PROFILE.value != 0 or helpers.DEBUG.value != 0 or helpers.BEAM.value != 0:
        raise LifecycleError("product requires JIT0/PROFILE0/DEBUG0/BEAM0")
    if repr(helpers.DEV) != "CUDA:PTX" or any(d.startswith("CUDA") for d in Device._opened_devices):
        raise LifecycleError("unselected or previously opened CUDA device")
    for name in ("IOCTL", "REGEN", "HCQ2", "CUDA_PTX", "CUDA_CC", "CUDA_MOCK", "MOCKGPU", "VALIDATE_WITH_CPU"):
        if helpers.getenv(name, 0): raise LifecycleError(f"unselected switch {name}")
    bindings = bindings or importlib.import_module("tinygrad.runtime.autogen.cuda")
    if getattr(bindings, "_ryeos_lifecycle", False): raise LifecycleError("lifecycle already installed")
    lifecycle = Lifecycle(bindings, selected_device, observe)
    def policy():
        if (repr(helpers.DEV) != "CUDA:PTX" or helpers.JIT.value != 0 or helpers.PROFILE.value != 0
                or helpers.DEBUG.value != 0 or helpers.BEAM.value != 0 or helpers.VALIDATE_WITH_CPU.value != 0):
            lifecycle._fail("selected product controls changed")
        for name in ("IOCTL", "REGEN", "HCQ2", "CUDA_PTX", "CUDA_CC", "CUDA_MOCK", "MOCKGPU"):
            if helpers.getenv(name, 0): lifecycle._fail("selected backend controls changed")
    lifecycle._policy = policy
    # The actual upstream atexit callback resolves Device[dn] before finalize.
    # Remove only this product's terminal CUDA entry from that Python traversal;
    # retained device/program/Buffer references remain fenced by the same owner.
    lifecycle._terminalize = lambda: Device._opened_devices.discard("CUDA")
    # All symbols, including forbidden ones, become guarded before consumers can
    # cache aliases. The native DLL escape is removed from the public module.
    for name in tuple(vars(bindings)):
        if name.startswith("cu") and len(name) > 2 and name[2].isupper() and callable(getattr(bindings, name)):
            lifecycle._hook(bindings, name, lifecycle._guarded(name))
    bindings.dll = None
    bindings._ryeos_lifecycle = True
    autogen = importlib.import_module("tinygrad.runtime.autogen")
    autogen.cuda = bindings
    sys.modules["tinygrad.runtime.autogen.cuda"] = bindings
    ops = importlib.import_module("tinygrad.runtime.ops_cuda")
    graph = importlib.import_module("tinygrad.runtime.graph.cuda")
    from tinygrad.engine.jit import _TinyJit
    from tinygrad.renderer.ptx import PTXRenderer

    def refused(*_args, **_kwargs): raise LifecycleError("path outside selected graph-free product")
    lifecycle._hook(graph.CUDAGraph, "__init__", refused)
    original_getitem = type(Device).__getitem__
    def getitem(self, name):
        canonical = self.canonicalize(name)
        if canonical not in ("CUDA", "CPU", "PYTHON"):
            raise LifecycleError("unselected device/backend")
        if canonical == "CUDA":
            with lifecycle.submission(): return original_getitem(self, canonical)
        return original_getitem(self, canonical)
    lifecycle._hook(type(Device), "__getitem__", getitem)
    original_jit = _TinyJit.__call__
    def jit(self, *args, **kwargs):
        if helpers.JIT.value != 0 or self.captured is not None: raise LifecycleError("JIT capture outside selected product")
        with lifecycle.submission(): return original_jit(self, *args, **kwargs)
    lifecycle._hook(_TinyJit, "__call__", jit)
    original_allocate = Buffer.allocate
    def allocate(self, opaque=None, external_ptr=None):
        if self.device.split(":")[0] == "CUDA":
            if opaque is not None or external_ptr is not None or (self.options and self.options.external_ptr is not None):
                raise LifecycleError("CUDA pointer adoption outside selected product")
            with lifecycle.submission():
                try:
                    return original_allocate(self, opaque, external_ptr)
                except BaseException:
                    lifecycle._poison = lifecycle._poison or "partial CUDA buffer registration"
                    raise
        return original_allocate(self, opaque, external_ptr)
    lifecycle._hook(Buffer, "allocate", allocate)
    original_deallocate = Buffer.deallocate
    def deallocate(self):
        if self.device.split(":")[0] == "CUDA":
            with lifecycle._lock:
                if (lifecycle._state in ("closed", "failed") or lifecycle._poison
                        or (lifecycle._state == "closing" and not getattr(lifecycle._local, "depth", 0))):
                    self._bufs.clear()
                    return
            with lifecycle.submission(): return original_deallocate(self)
        return original_deallocate(self)
    lifecycle._hook(Buffer, "deallocate", deallocate)

    original_device = ops.CUDADevice.__init__
    def device(self, name):
        if name not in ("CUDA", "CUDA:0") or ops.CUDADevice.devices: raise LifecycleError("extra CUDA device")
        with lifecycle.submission():
            try:
                original_device(self, name)
            except BaseException:
                lifecycle._poison = lifecycle._poison or "partial CUDA device construction"
                raise
            self.renderers = [PTXRenderer]
            self.graph = None
            self.allocator.supports_transfer = False
    lifecycle._hook(ops.CUDADevice, "__init__", device)

    def wrap(owner, name, validate=None):
        original = getattr(owner, name)
        @functools.wraps(original)
        def guarded(self, *args, **kwargs):
            with lifecycle.submission(), lifecycle._lock:
                if validate is not None: validate(self, *args, **kwargs)
                try:
                    return original(self, *args, **kwargs)
                except BaseException:
                    lifecycle._poison = lifecycle._poison or f"partial CUDA operation {name}"
                    raise
        lifecycle._hook(owner, name, guarded)

    def allocator(self, *_args, **_kwargs):
        if (type(self) is not ops.CUDAAllocator or self.dev not in ops.CUDADevice.devices
                or getattr(self.dev, "allocator", None) is not self):
            raise LifecycleError("unselected allocator")
        options = _kwargs.get("options")
        for arg in _args:
            if hasattr(arg, "external_ptr"): options = arg
        if options is not None and options.external_ptr is not None: raise LifecycleError("external CUDA pointer")
    def offset(self, buf, size, offset):
        if offset < 0 or size <= 0: raise LifecycleError("invalid CUDA view")
        lifecycle._range("device", _value(buf) + offset, size)
    for name in ("alloc", "free", "free_cache", "_alloc", "_copyin", "_copyout", "_offset"):
        validate = allocator if name in ("alloc", "free", "_alloc") else offset if name == "_offset" else None
        wrap(ops.CUDAAllocator, name, validate)
    lifecycle._hook(ops.CUDAAllocator, "_transfer", refused)
    original_free = ops.CUDAAllocator._free
    def free(self, opaque, options):
        with lifecycle._lock:
            if lifecycle._state in ("closed", "failed"):
                return  # finalization is inert, inventory uncertainty is retained
        with lifecycle.submission(): return original_free(self, opaque, options)
    lifecycle._hook(ops.CUDAAllocator, "_free", free)
    for name in ("synchronize", "runtime", "_select_renderer"):
        wrap(ops.CUDADevice, name)
    for name in ("__init__", "__call__"):
        wrap(ops.CUDAProgram, name)
    def destroy_program(self):
        with lifecycle._lock:
            module = getattr(self, "module", None)
            if module is None or ("module", _value(module)) not in lifecycle._resources: return
            if lifecycle._state != "open" or lifecycle._poison: return
        with lifecycle.submission(): lifecycle._call("cuModuleUnload", (module,))
    lifecycle._hook(ops.CUDAProgram, "__del__", destroy_program)
    lifecycle._hook(ops.CUDADevice, "finalize", lambda self: None)
    return lifecycle
