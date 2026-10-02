# Pinned graph-free CUDA producer map

Source checkpoint: tinygrad 4c206a52b1a72a98db8c97576959b54fa2a38232,
clean shared checkout inspected read-only. D base d17f059bbca83702488380e25df872404d40d192.
The prior ops_cuda/device/graph/helpers hashes match this checkout. This is a
source map and cooperative product implementation, not driver qualification.

## Request and lifetime chain

The CPU session is reference material, not the GNU entry point. Runtime owns the
separate GNU bootstrap/session. Its exact DLL resolver must precede generated
bindings, then lifecycle installation must precede ops_cuda/graph/model imports.
Model construction and each complete Worker.execute call belong inside
lifecycle.submission(). Session retirement calls shutdown after closing inbox
admission and quiescing requests, including constructor failure and EOF/error.

session.Worker validates/tokenizes a request, iterates QwenModel.generate and
publishes final/error using RequestInbox.complete. QwenModel retains read-only
safetensors mappings and CPU Tensor.from_blob sources, GPU-bound lazy state and
weights across requests. generate creates a fresh Transformer sharing those
weights for each request: its attention KV caches live across tokens of that
request, not across separate requests. precompute_freqs_cis is functools-cached
and can retain rotary buffers across requests. The otherwise unused initial
Transformer and tinygrad runtime_cache can also survive request completion.
Token inputs, sampling temperature, activations, returned token and fresh KV
state eventually deallocate through Buffer.__del__; deallocation commonly puts
native allocations in LRUAllocator.cache instead of freeing them. Views carry
offset pointers without owning an additional native allocation.

Transformer.__call__ chooses prefill_jit/rollout_jit. _TinyJit.__call__ with JIT0
still prepares inputs, runs forward, and realizes results. FFNBlock.__call__ uses
function(precompile=True, allow_implicit=True); this is kernel precompilation,
not CUDA graph capture. Tensor.realize leads through schedule compilation and
engine.realize.run_linear -> compile_linear/to_program -> PTXRenderer and
PTXCompiler.compile (pure PTX text substitutions). get_runtime caches actual
CUDAProgram objects. CUDAProgram loads a CUmodule before cuModuleGetFunction and
optional cuFuncSetAttribute; acquisition must be tracked before either can fail.
exec_kernel allocates Buffer objects and invokes CUDAProgram.__call__ ->
cuCtxSetCurrent -> cuLaunchKernel on the default stream. compile_linear's
optimize_local_size may call wait=True even JIT0/PROFILE0, creating two events.
These must be inventoried; simply rejecting timed launches breaks this path.

CUDADevice.__init__ calls cuInit/cuDeviceGet, creates a regular context BEFORE
compute-capability queries, peer setup, registration and Compiled initialization.
Native context ownership cannot depend on CUDADevice.devices. CUDAAllocator
allocates regular device memory or portable (flags 0x01) host memory; _copyin
appends pinned staging memory to pending_copyin before memmove/asynchronous HtoD.
_copyout -> synchronize_system -> each device.synchronize -> allocator.free for
pending buffers -> LRU cache, then synchronous DtoH. .item materializes a token
through this copyout; it does not discharge weights/modules/caches or the context.

Request final/error is only request completion. There is no CUDA close in the
reference CPU main loop. Buffer and module finalizers and device's atexit finalize
are separate lifetimes; suppress_finalizing may hide their errors at interpreter
exit and cannot establish retirement. Lifecycle wraps all generated cu* bindings
before aliases exist, records native acquisitions before returning to tinygrad,
and guards allocator/Buffer/device/JIT/renderer paths. Terminal finalizers become
in-memory no-ops; the terminal CUDA entry is removed from Device._opened_devices
before observing success so the actual upstream atexit traversal is inert.
Cached guarded native aliases still refuse every terminal call.

## Closed native inventory and close sequence

Exactly one regular context for selected CUDA ordinal (initial product ordinal
0), ordinary cuMemAlloc_v2, portable cuMemHostAlloc, CUmodule/function mappings,
and timing CUevents. Pending/LRU allocations remain in the native ledger even
when their Python owners are cached or their constructors fail. Other cu* entry
points are refused, including graphs/execs, IPC/imports, primary/shared contexts,
peer access, streams, asynchronous/pool/virtual allocations and raw proc lookup.
CUDA opaque/external adoption and unselected allocator/device/renderer choices
refuse, while CPU mmap/from_blob still forwards unchanged. CUDA DtoD transfer's
source event is not destroyed by upstream; transfer is excluded in this first
product, and supports_transfer=False prevents selection by exec_copy. CPU staging
is the allowed copy path. PROFILE must remain 0; DEBUG/BEAM/development backend
switches remain closed. PTX local-size timing is allowed and checked.

Shutdown: close submissions, require no active submission, validate hooks and
inventory, set the owned context current, synchronize, destroy surviving events,
unload modules, free ordinary device allocations, explicitly free every portable
host allocation, destroy the regular context. Each successful release removes
only that native record. A failed release keeps its record and poisons outcome;
no retry can produce success after uncertainty. Context destruction is never
used to excuse a failed portable host release. Buffers/programs/caches surviving
close cannot make a later native call. No CUDA timeout or process supervisor is
implemented. D supplies original deadline/channel/process-group/financial joins.

## Deferred graph product and acceptance

graph.cuda creates graph, memcpy/kernel nodes, graph-exec instance; update/replay
calls mutate executable nodes, launch and possibly create timing events. Its
__del__ destroys graph and exec separately. Enabling it requires partial graph/
exec/node acquisition tracking, dependency ownership, replay admission, checked
update failures, graph-exec-before-graph retirement and real optimized numerical/
performance qualification. No such expansion is implemented here.

The adapter keeps the pinned Transformer, PTX compilation and native kernel
path usable. Injected-driver tests cannot compute GPU numerics; real eight-token
Qwen compatibility and full-prefix logits against a retained independent numeric
reference are unrun. ARC model choice, optimized inference and useful throughput
need separate measured evidence. Actual driver/nvproxy/installed admission,
authenticated private observation consumption, independent group retirement and
financial settlement remain D/coordinator acceptance, never supplied by this map.

## Agreed source interface and offline limits

D's private interface record was fully read and re-ACKed after its identity
clarification. Runtime and D ACK install(*, bindings=None, selected_device=0,
observe=None) -> Lifecycle; submission() covers import/construction and whole
Worker.execute; shutdown() returns frozen ShutdownResult(status busy/failed/closed,
observation_delivered, error). Busy closes new admission; only the existing owner
quiesces and retries. A native error or partial constructor poisons the owner;
its acquired handles or explicit uncertainty remain and no success is delivered.
Duplicate terminal shutdown returns the identical result without native/callback
replay. Callback failure is failed/undelivered even if native close already finished.

observe receives frozen CloseObservation with selected ordinal and release counts
(context/device/portable-host/module/event); no handles. These are local semantics,
not the frame or authentication. Offline absent observe may return locally closed
but observation_delivered remains false. Production runtime requires D's channel
owner before loading CUDA. It validates init, then D's matching shutdown, and
sends the single strict <=4096-byte length-prefixed retired frame followed by EOF.
Only version/kind/teardown/capsule_hash/cleanup_contract/process_owner/resource_set
are allowed. capsule_hash is the exact admitted capsule CAS key; process_owner and
resource_set use existing ExecutionProcessIdentity digests; teardown is the
ORIGINAL resource reservation ID/live one-use slot. Init is queued after held
birth/durable attachment before release, without awaiting a child response.
D closes/quiesces session requests before sending shutdown. Early EOF/error can
attempt local close but cannot authorize publication. D consumes through its
unique held channel under the original deadline, validates trailing/duplicate/
foreign/late/partial data, and joins independent group and financial settlement.

The complete runtime must bind CUDA ordinal0 to the same admitted observed
character-device selections; ordinal0 alone proves no hardware identity.
Lifecycle performs no FD reads/writes, clocks, waits, host thread creation, process
control, filesystem discovery, credential acquisition or provider operations.
Its RLock/thread-local state only serializes in-memory admission/native operations.
A blocked driver call remains blocked; no Python timeout establishes retirement.

Injected tests run the exact upstream generated bindings with a fake DLL adapter,
real CUDADevice/CUDAAllocator/LRU/Buffer/CUDAProgram/timing helper, real PTX tensor
compilation/realization/runtime cache, GC/finalizers and actual device atexit
callback. Host memory is harness-owned ctypes storage. No CUDA/native DLL loading,
GPU calls, compiler children, weight downloads/execution or provider contact occurs.
The real CPU Tensor.from_blob/Buffer/CPUAllocator._alloc path is checked with CPU
initialization injected: upstream CPUDevice opens semaphores, creates staging mmap
buffers and compiles native service programs; these paths remain unexercised.
ops_cpu.py:104 directly loads libgcc_s.so.1 via ctypes.CDLL, outside support.c.DLL;
this concrete runtime closure gap was sent to GPU-runtime, which owns the selected
GNU library composition and qualification. CPU source is unchanged.

Real Transformer/Qwen weight execution, full-prefix numerical comparison, model
suitability and throughput remain unrun. Runtime owns the joined actual Worker/
QwenModel.generate-to-pinned-Tensor request and shutdown test from its complete
staged source closure; a synthetic driver cannot establish numerical accuracy.
No authenticated daemon evidence or installed M1 acceptance follows from these
offline tests. Driver/nvproxy/ABI/hardware mapping and funded installed acceptance
remain required.
