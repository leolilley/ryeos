# GNU/CUDA NVML product-qualification source seam

Status: **unsigned consumer preparation; no NVML provider, producer output,
node occurrence, or qualification policy is selected.** This note and
`lib/local-tinygrad-gnu/nvml_product_qualification.py` prepare the product edge;
they do not create a daemon workflow, a verifier grant, or an admitted device
fact.

## Current immutable identities and missing bytes

The current source pins are GNU producer input manifest
`ec7d4a2a749b0b709dabca0a7919c3a897a9b671be56332b67a659b6bd82cac6`, tinygrad
tree `e468f263924eb44fc21405dafba8ea27a7cd9e70dd6847822905a652549be21c` at
revision `4c206a52b1a72a98db8c97576959b54fa2a38232`, musl compiler-support tree
`800d4969489634cc3bbc5774bd9e99a330cdc23bbc1fd0fd231ec6a88ca9acdf`, toolchain
tree `0f278c1f855f123cbab94e6c1aa11dc0ae0b64b27a73241a4f67941d12c70e53`, and
Qwen3-0.6B model tree `1a75084b7404ba3409e598a8ee2d09a8d236edf1fa5f33917c3b3a8f0494e35b`.
The model's seven verified files exist in the private preparation directory; this
is source preparation, not a destination receipt or lease. Tinygrad/toolchain/
runtime hashes are immutable references, not proof of destination-local bytes.

The GNU output is not yet available as a retained product: the producer's
`chain_root_id`, `thread_id`, output receipt, receive/bind result, and destination
witness are absent. The input manifest is not the output manifest. The Worker
draft's driver digest remains `null`; there is no selected immutable NVML/driver
tree, `libnvidia-ml.so.1` hash, recursive dependency closure, or destination
lease. No driver or NVML digest is supplied here.

The cleanup owner has prepared
`cuda_lifecycle/nvml_observer.py::observe_assignment` at combined integration
checkpoint `55502ab64`; the observer bytes are
SHA256 `c0cd6dec81c3e46f17607f1ae13440a2abfda2eb982d9c1be338bf084509403f`,
introduced by cleanup commit `fd0940f43df6342146d409351c1cf419495b438e` and
extended with explicit L4 profiles in `d8d16c1b4e49d44aeed2c5409aa3a75e50da5f7a`.
That helper takes injected bindings and the whole raw `ExecutionResourceSelection`;
it does not discover or load NVML. Product source in this worktree is based on D checkpoint
`d17f059bbca83702488380e25df872404d40d192`; final integration must include the
cleanup owner's exact module and regenerate the complete worker-source digest.

## Product-qualification edge

`nvml_product_qualification.qualify_raw_assignment` consumes the existing
`ProductQualificationPolicy.v3` static `verifier_parameters`, D's raw retained
selection, and the exact observer/provider API injected by the accepted
qualification owner. It delegates native query interpretation to the cleanup
owner's helper and returns the existing
`ryeos.product_qualification_result.v1` envelope, binding its candidate to the
selected subject manifest. A verifier policy can use this exact finite parameter
shape:

```yaml
schema: ryeos.local_tinygrad_nvml_assignment_policy.v1
primary_device_role: <signed retained primary role>
l4_query_policy: null # or one exact qualified L4QueryPolicy value
```

The role and query profile are unresolved until D supplies the signed raw role
and an exact native qualification chooses one L4 interpretation. `null` means
the helper's strict success-only MIG queries; it is not fallback behavior. The
result claim `nvidia_nvml_primary_assignment_v1` is only a verifier output shape
until a selected signed ProductQualificationPolicy allows it and a real native
attempt passes.

The current GNU private verifier/policy covers only GNU startup providers and
zlib. It is not modified or extended with GPU claims. The existing qualification
launch and result owners remain the path for recording a selected verifier's
`probe_evidence`; the caller must deliver the raw selection and exact injected
provider from D's same accepted owner. The product helper itself cannot convert
candidate JSON into `ExecutionResourceSelection` facts or construct D's
init-v2 startup issuer.

This source cut does not declare a new Tool item or verifier relationship: no
driver/NVML product producer or source-closure binding for reusing the worker
helper from a ProductQualification Tool has been selected. The API is ready for
the existing qualification owner to call once that exact signed verifier source
closure is chosen. Keep use on `launch-product-qualification` and its v3 policy;
do not add a second endpoint or let the verifier discover a helper from the
installed host tree.

## Exact provider closure requirements

The verifier must use one selected, absolute `libnvidia-ml.so.1` member from an
admitted driver/NVML product and pass its exact bindings to the observer. It
must not use `find_library`, `pynvml`, a bare soname, ambient `LD_LIBRARY_PATH`,
or a host-site package. The selected provider tree must include and qualify all
transitive `DT_NEEDED` objects, symbol versions, RUNPATH/RPATH behavior, GNU
loader resolution, and any later `dlopen` dependencies. A missing-provider
negative check must fail before NVML initialization even with ambient candidates
present. Those bytes and their source producer are currently unselected.

The raw measurement joins exactly one retained NVIDIA primary character-device
identity to the same opaque NVML handle's minor and UUID. The NVML result is a
candidate, not node policy, accepted facts, resource settlement, or a CUDA
cleanup receipt. D owns the typed caller, descriptor grant, same-occurrence
funding/deadline, accepted evidence-to-resource-facts join, and
`CleanupStartupComparisons` issuer. GPU-cleanup's runtime continues to compare
that accepted UUID against the same actual `CUdevice` handle before context
creation. No source change here adds an alternate channel or second owner.

Adding this consumer module changes the private Worker source tree. Its previous
draft digest is not carried forward as a claim about the enlarged tree; the
coordinator regenerates the digest and complete manifest from the final joined
source before signing. No tests, builds, native library loads, GPU/provider
operations, production, signing, or activation were performed for this slice.
