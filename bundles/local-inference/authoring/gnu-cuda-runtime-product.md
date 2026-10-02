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
this source task. Resolve actual GNU production
through service:external-content/product with exact chain_root_id, thread_id,
recipe_binding=product_recipe, product_name=runtime. Reuse existing product
receive/qualification/bind services and the new draft private-consumer
relationship. Do not use the input manifest as the output manifest. The existing
fixed `/ryeos` qualification does not qualify this private descriptor consumer.
The original producer's product includes python/, notices and ELF inventory;
no producer change or alternate production system is introduced here.

On 2026-10-02, a coordinator-authorized private model preparation reused the
five hash-verified local model/tokenizer leaves and downloaded only the immutable
revision's LICENSE (11,343 bytes, SHA256
832dd9e00a68dd83b3c3fb9f5588dad7dcf337a0db50f7d9483f310cd292e92e),
with a 64 KiB acquisition ceiling and 30-second timeout. The existing release
author's build_model function ran offline and generated ORIGIN.json; its existing
realization_manifest function reproduced the exact model manifest in the table:
seven regular files, 1,514,745,854 bytes. Preparation data stayed below the selected
1.6 GB storage ceiling. No weights were downloaded or executed and the original
vendor files were preserved. The private operation-result.json records the inputs,
member digests and successful result; this is preparation evidence, not a retained
node product, destination witness, qualification, upload or activation.

The smallest GNU handoff still requires the actual producer chain_root_id and
thread_id, product_recipe/runtime receipt, authenticated output manifest and a
destination retention lease. The existing graph selects production-support through
prepared_inputs_to_gnu_python_production; the tool uses the pinned musl producer
runtime and exact GNU archive inputs. Recorded Oct1 normal-service evidence could
not retrieve the historical GNU witness and located no current producer coordinate.
Its threadless validate --current-head attempt refused project_source_push_first;
this is historical evidence, not a fresh observation of the installed operator.
An exported receipt from the producer owner is the next input. Historical input
pins, fixed-root startup proofs and host preparation cannot replace that receipt
or independently qualify the private consumer. No new producer or harness is used.

## Private shutdown interface

D/Lillux creates, inherits and retains the separate duplex cleanup channel on
RYEOS_RESOURCE_CLEANUP_FD, distinct from session/stdout/stderr. The worker
consumes init before provider startup/model/CUDA. Frames are four-byte
big-endian lengths 1..4096 and strict JSON. Init is version 2 and has exactly
`version`, `kind`, the five existing bindings (`teardown`, `capsule_hash`,
`cleanup_contract`, `process_owner`, `resource_set`), `uuid_v2_hex`, `gnu_bounds`
and `driver_bounds`. The UUID is 32 lowercase hex characters and nonzero. Each
bounds object has exactly the positive integer fields `maximum_entries`,
`maximum_depth`, `maximum_file_bytes`, and `maximum_total_bytes`, with the file
bound no larger than the tree total. The Python consumer returns a frozen typed
record; the runtime requires its UUID and both typed bounds before path scan or
provider contact. Shutdown and retired remain strict version 1 and repeat only
the five original bindings; shutdown must exactly match init's five bindings.
D must derive every value from the retained original reservation, exact capsule CAS
key, cleanup agreement, held process owner, selected product manifests and
independent physical-device observation. Matching JSON authenticates nothing.

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
truncates. The existing check_prompt_packing.py utility observed 20 input tokens
and eight reserved output tokens (28 combined) from the exact local metadata,
using host Python 3.14.7 and Unicode database 16.0.0. Rendered input is 108 bytes,
SHA256 772f67df16bab09eedbf33706ff64c165f59713644dc92e3c22d4485e4d1a142;
the token-ID digest is
f27d77e1b41d9c360c3245cec811c8340903ce9c32882331573e0d7d59db7590.
CPU and GNU tokenizer/contract/profile source bytes match. GNU destination
tokenizer parity remains UNOBSERVED; local packing, synthetic fixture counts and
older 4B numeric evidence do not supply GNU 0.6B numerical qualification.
Actual GNU/native/compiler/driver/device qualification, weights, provider startup,
signing, production, destination admission, cloud activation and paid M1 are UNRUN.

## Offline source checks

The complete private closure includes GPU-cleanup commit 02c304246 (runtime
cherry-pick cc1bbf34f). Its 20 injected-driver controls exercise actual pinned
tinygrad Python device/program/allocator/PTX/tensor paths. The runtime suite has
25 controls including actual session.main -> Worker.execute -> QwenModel.generate
-> real Tensor/PTX kernel path -> the same lifecycle shutdown. That joint test
injects a synthetic forward and CPU device initialization: it executes no model
weights/native library/compiler child and establishes no numerical accuracy.
The lifecycle import resolves from this checkout's actual GNU source closure.
16 bootstrap controls and five unchanged CPU inbox controls also pass. Parser
and layout tests remain distinct from actual mapped-provider/ABI qualification.

Frame EOF is clean only before the first byte of a new four-byte header.
Partial headers and payloads raise a protocol failure through the actual inbox
and main loop even after independently authorized successful cleanup. Three real
socket fixtures cover clean closure and truncated headers/payloads with the
matching private shutdown command and retired frame; retirement never changes
that failed execution into success.

## 2026-10-02 source follow-up

The private source branch adds a bounded structural check for the selected
driver's recursive ELF `DT_NEEDED` closure and wires it into GNU worker startup.
The check accepts only x86-64 ELF shared objects inside the separately selected
GNU and driver `lib/` roots, enforces each retained tree's explicit entry/depth/
file/total-byte bounds, rejects escaping or ambiguous providers, and checks that
each dependency resolves through the loader's first exact search entry. It
supports explicit `DT_RUNPATH`; inherited `DT_RPATH` is refused. It reads ELF
metadata only: it does not load native code, qualify symbol versions/ABI,
observe mapped providers, cover future `dlopen` calls, or establish a device.

The strict cleanup consumer now exposes init-v2's UUID and two typed bounds on
the existing channel. `run_channel_product` dispatches those exact values to
`run_product`; `prepare_environment` checks both tree bounds before path
discovery and provider contact. GNU bounds may not exceed the unsigned
`gnu_runtime_to_private_worker` draft ceiling; that ceiling is not
selected-product evidence and is never a default. The runtime validates the
UUID before loading the selected generated tinygrad CUDA binding, calls
`cuInit(0)` and requires `cuDeviceGetCount` to report exactly one visible device,
then passes UUID-v2 to `install(expected_uuid_v2_hex=...)`. Lifecycle checks
UUID-v2 on the exact `CUdevice` handle returned for ordinal zero before that
handle reaches context creation. A missing, malformed, mismatched, zero-device,
multiple-device or failed count comparison refuses before a CUDA context is
created. These source checks do not prove independent D authority, a retained
lease, physical close, or native qualification.

The current driver draft still has no selected tree, bounds, or digest, and D's
protected producer has not yet supplied original-owner-bound product/device
selections. Therefore production startup remains fail-closed until D wires that
selection contract; do not pass these values through ambient environment or
user JSON.

The source cuts are `1e7dd0e7b`, `976a4dfc8`, `1e083fbdf`, `0e98a1d3e`,
`f8258ecc8`, cleanup init-v2 `4ff6c19c1` (rebased as `9fa36a92a` in the
runtime worktree), runtime dispatch `3bbb621e3`, same-handle guard wiring
`fbdaaaab4`, exactly-one-visible-device enforcement `51e99d5f6`, and lifecycle
documentation `1ace6898b`. They are based on D checkpoint `3b89173e7` and the
coordinator-reviewed combined source. The GNU runtime suite passes 40 tests;
the cleanup-init-v2/device-lifecycle suite passes 64 tests. These fixtures
establish parser and source-order/refusal behavior only. No native driver,
compiler child, model weight, CUDA device or provider was loaded or run; all ABI,
actual GNU production, destination admission, native qualification, signing and
activation gates remain open.
