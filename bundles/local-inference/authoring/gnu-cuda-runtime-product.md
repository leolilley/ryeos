# GNU CUDA graph-free compatibility product — unsigned source

The distinct `qwen3-0.6b-gnu-cuda-graph-free-2048-draft` worker runs the
existing persistent-session/Qwen implementation under selected GNU Python.
CPU workers and their signed source remain unchanged. This product is a
compatibility probe, not an ARC model choice or optimized TinyJit product.

## Source composition

`lib/local-tinygrad-gnu/` contains bootstrap, runtime, cleanup_channel,
bootstrap_support, session, compiler, cpu_support, model, model_contract, tokenizer,
model-profiles/qwen3-0.6b.json and the GPU-cleanup-owned `cuda_lifecycle/`.
Compiler/model/model-contract/tokenizer/profile start as exact copies of
`lib/local-tinygrad/` at D checkpoint
`d17f059bbca83702488380e25df872404d40d192`; CPU source digest is
`8c126ea16198349d77579d7c9f0eccd84c0139e1b30e39805cce5f5b0dd5ccf6`.
Session keeps the same wire, inbox, completion and output implementation;
only startup, lifecycle scopes and compatibility request limits change.
bootstrap_support is the checkpoint's finite authoring helper admitted as
product source. No runtime import refers to authoring/, vendor/, another
worktree, proposal files or host site-packages.

The executor retains source/external inputs and original private scratch.
Required workspace roots are python-gnu, runtime (musl compiler support),
toolchain, tinygrad, model, driver and scratch (home/cache/tmp). The launcher
executes the selected GNU loader with --inhibit-cache, explicit --library-path,
Python home and -P -S -B startup. PATH is empty; runtime rebuilds import paths
from the selected GNU stdlib/dynamic extensions and its exact worker/tinygrad
roots. It verifies interpreter prefix/flags/hash seed and module origins.
The tinygrad DLL resolver binds only selected GNU libc and driver entries;
unused nvrtc/nvjitlink remain unbound and other discovery refuses.
The reachable CPU weight-staging import directly loads libgcc_s.so.1 outside
that resolver. cpu_support binds this finite import to selected
python-gnu/python/lib/libgcc_s.so.1, already in GNU production compatibility
files and interpreter DT_NEEDED. Missing/load failure or wrong source/handle
refuses; native ABI/transitive loading and CPU service kernels remain UNRUN.

Lifecycle installs before model/CUDA consumers, encloses model import,
constructor and each complete request, and closes on the same session's exit.
Its real native acquisition ledger includes partial constructors, module/function
resolution, buffers, cached/pending portable host memory and PTX timing events.
JIT0/PROFILE0 still allows tensor kernel compilation and tuning events.
CPU mmap/from_blob weight sources are preserved.

## Exact dependencies and available evidence

| Role | Source identity | Missing destination selection |
| --- | --- | --- |
| GNU Python | graph:ryeos/development/gnu-python-production, product_recipe/runtime; input tree ec7d4a2a749b0b709dabca0a7919c3a897a9b671be56332b67a659b6bd82cac6 | Actual retained produced output coordinate, receive/bind, destination witness and private-consumer qualification |
| Compiler support | musl tree 800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf | Actual destination realization/lease |
| Toolchain | 0f278c1f855f123cbab94e6c1aa11dc0ae0b64b27a73241a4f67941d12c70e53 | Actual destination realization/lease |
| tinygrad | 4c206a52b1a72a98db8c97576959b54fa2a38232, tree e468f263924eb44fc21405dafba8ea27a7cd9e70dd6847822905a652549be21c | Actual destination realization/lease and selected driver ABI |
| Model/tokenizer | Qwen3-0.6B c1899de289a04d12100db370d81485cdf75e47ca; tree 1a75084b7404ba3409e598a8ee2d09a8d236edf1fa5f33917c3b3a8f0494e35b | Actual destination realization/lease; 1,503,264,768 BF16 tensor bytes |
| Driver/device | selected driver/lib/libcuda.so.1 plus complete transitive closure | Actual immutable driver tree/ABI, single observed device and exact ordinal0 relation |

Source declarations and historical archive/manifests are available references.
No produced GNU or destination runtime/model/device witness was selected by
this source task, and no payload was downloaded. Resolve actual GNU production
through service:external-content/product with exact chain_root_id, thread_id,
recipe_binding=product_recipe, product_name=runtime. Reuse existing product
receive/qualification/bind services and the new draft private-consumer
relationship. Do not use the input manifest as the output manifest. The existing
fixed `/ryeos` qualification does not qualify this private descriptor consumer.
The original producer's product includes python/, notices and ELF inventory;
no producer change or alternate production system is introduced here.

## Private shutdown interface

D/Lillux creates, inherits and retains the separate duplex cleanup channel on
RYEOS_RESOURCE_CLEANUP_FD, distinct from session/stdout/stderr. The worker
consumes init before startup/model/CUDA. Frames are four-byte big-endian lengths
1..4096 and strict JSON with exactly version=1, kind, teardown, capsule_hash,
cleanup_contract, process_owner and resource_set. Every binding is 64 lowercase
hex; shutdown must repeat init exactly. D derives these from the original
reservation, exact capsule CAS key, cleanup agreement and actual held process's
owner_incarnation_digest/resource_set_digest. Matching JSON authenticates nothing.

D queues init after held birth/durable attachment before release without waiting
for worker response. D closes session request input/quiesces then sends shutdown
under the original deadline. Worker receives that matching command before
retirement success publication. The lifecycle's callback emits one binding-only
retired frame and closes the descriptor for EOF only after complete native close
and terminal fencing. Missing/invalid shutdown still invokes local close, but
cannot emit success. Partial write/delivery failure cannot replay. D owns bounded
parent reads/writes, original deadline, rejection of trailing data, actual
admission, independent group retirement, accepted evidence and financial joins.

Operation ownership: this product interprets the inherited session/cleanup wire
using the existing session read/write pattern; it creates no process/socket,
clock, timer, journal or supervisor. Compiler.py reuses the existing exact musl
compiler invocation/subprocess and scratch mechanism. Children remain in the
existing Lillux-owned process group. This source reuse does not qualify
compiler-child retirement or complete any outstanding host capability migration.
The product has no independent process or financial settlement authority.

## Activation and unresolved qualification

All worker/provider/activation/relationship/smoke files are distinctly named
unsigned drafts. Source/driver digest, original timeout, spend and qualification
policy remain explicit unresolved fields; never sign/publish them as they stand.
Partial archive activation prepares only existing musl/tinygrad/compiler/model
inputs; GNU produced output and exact driver admission remain separate prerequisites.
D supplies the signed trusted cleanup/session contract and typed device requirement.
The separate gnu_cuda_persistent_session protocol retains the same wire but
requires trusted_process_group and its independently admitted resource cleanup
digest. Integration must add the exact new protocol to Core worker.kind-schema's
closed allowlist; neither a CPU protocol nor a filename alias supplies GPU authority.
The coordinator integrates actual lifecycle source, regenerates the complete
source digest/manifests and signs through the supported workflow only with authority.

Before executable admission: qualify GNU ELF DT_NEEDED/symbol versions,
loader independence, fixed RUNPATH/cache/default/hwcaps searches, Python native
extensions including ctypes/libffi, every dlopen provider and selected driver
transitive dependencies. Exercise missing-provider refusal with ambient candidates
present. Qualify exact compiler children separately; direct GNU invocation does
not bypass their PT_INTERP. A selected loader path alone closes none of these.
See gnu-descriptor-qualification.md for the original finite route and alternatives.

Prepared exact request bytes are authoring/contracts/
qwen3-0.6b-gnu-cuda-compatibility-request-v1.json, SHA256
4070d0608443674275f22745e563ae770abde5edeed013c860108649c0e96f0e.
Worker refuses more than 256 packed input tokens, more than eight output tokens,
input+output greater than 2048, sampling/seed changes, thinking or tools; it never
truncates. Actual exact tokenizer packing remains UNOBSERVED. Synthetic fixture
counts and older 4B numeric evidence do not supply GNU 0.6B numerical qualification.
Actual GNU/native/compiler/driver/device qualification, weights, provider startup,
signing, production, destination admission, cloud activation and paid M1 are UNRUN.

## Offline source checks

The complete private closure includes GPU-cleanup commit 02c304246 (runtime
cherry-pick cc1bbf34f). Its 20 injected-driver controls exercise actual pinned
tinygrad Python device/program/allocator/PTX/tensor paths. The runtime suite has
22 controls including actual session.main -> Worker.execute -> QwenModel.generate
-> real Tensor/PTX kernel path -> the same lifecycle shutdown. That joint test
injects a synthetic forward and CPU device initialization: it executes no model
weights/native library/compiler child and establishes no numerical accuracy.
The lifecycle import resolves from this checkout's actual GNU source closure.
16 bootstrap controls and five unchanged CPU inbox controls also pass. Parser
and layout tests remain distinct from actual mapped-provider/ABI qualification.
