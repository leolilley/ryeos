# Local inference bundle

Local-model execution content for RyeOS. The bundle owns concrete worker
programs, their adjacent source, provider routes, activation fixtures,
acceptance probes, and model-domain knowledge. The generic `worker` kind,
persistent-session protocol, content stores, effect records, and execution
identity remain platform-owned.

The ordinary full source installation includes this bundle:

```text
sudo scripts/pkg/install-local-direct.sh --populate --all --trust-source-publishers
```

Source publication validates the signed YAML contract with Python 3 and
PyYAML (`python3-yaml` on Debian/Ubuntu). Realization release authoring also
requires `zstd` to extract the exact python-build-standalone licensing metadata
artifact; release and container builders install their required dependencies
explicitly.

The release pipeline keeps third-party runtime/model execution in read-only,
no-secret jobs. Fresh write-authorized jobs accept only the exact regular-file
asset set whose bytes match the source-owned contract, and never execute those
assets. Disabled-isolation activation and replay are functional/recovery
conformance evidence over bytes the RyeOS publisher has already chosen to
trust; same-UID observations are not evidence that adversarial code is safe.

Bundle-specific publisher authoring lives under `authoring/`, not in RyeOS's
generic release scripts and not in the admitted `.ai` runtime closure. The
reviewed Qwen3-4B source contract can be materialized from its immutable
upstream revision with:

```text
python3 bundles/local-inference/authoring/author_model.py \
  --contract bundles/local-inference/authoring/contracts/qwen3-4b-bf16-model-source-v1.json \
  --cache /path/to/digest-cache \
  --output /absent/output/qwen3-4b
```

The pre-observation numeric reference contract is
`authoring/contracts/qwen3-4b-bf16-numeric-oracle-request-v1.json`. It freezes
the independent reference procedure and comparison policy only; it contains no
observations and grants no activation, publication, qualification, or ARC
acceptance authority. Focused authoring tests live in `tests/authoring/`.

The independent CPU oracle and the pinned tinygrad `CUDA:PTX` candidate have
now been compared through the consequence-free authoring utility
`authoring/compare_numeric_oracle.py`. The compact retained evidence is
`authoring/evidence/qwen3-4b-bf16-numeric-conformance-v1.json`; it binds the
exact request, model manifest, reference environment, candidate source,
compiler closure, and observed device. All three cases matched their greedy
tokens and complete top-16 sets within the predeclared error bounds. The
candidate ran in Modal's Python 3.12 image process; the admitted runtime's musl
loader/libraries served the exact compiler closure but its Python interpreter
did not run the model. This establishes numeric conformance only. It is not
runtime ABI, target, hardware, isolation, worker/provider, publication, or ARC
acceptance qualification.

A follow-up operator calibration ran the same candidate under the admitted
musl Python 3.14 runtime. Binding tinygrad's POSIX support to that runtime's
exact `libc.so` was valid, but Modal's observed NVIDIA driver library then
failed to load because it requires the glibc symbol `gnu_get_libc_version`.
The CPU workers keep their musl runtime. A GPU worker must instead compose a
separately admitted GNU runtime and qualify its complete driver ABI; it must
not fall back to the host interpreter or ambient library search.

An executable local-model profile is the signed worker composition, not a
model name selected by Python code. It binds exact immutable products for the
Python runtime, tinygrad source, compiler/toolchain and model/tokenizer, plus
the admitted worker source and its backend, numeric and resource contract.
The source-owned Qwen-family semantics are bounded data under
`lib/local-tinygrad/model-profiles/`; they do not resolve products or grant
activation. A project such as ARC selects one already-qualified provider and
worker profile as a unit. It does not independently choose ambient Python,
tinygrad, toolchain or model versions, and it cannot combine an ARC-trained
derivative with a different execution closure without new qualification and
promotion.

On the default trusted single-user node, RyeOS delivers the exact signed source
and external realizations through a daemon-owned private workspace and runs the
persistent worker under disabled OS isolation. Explicit node policy may select
Core's native Lillux backend for stronger confinement; RyeOS records whether
confinement and isolated networking were actually enforced.

Installing bundle bytes does not acquire model/runtime content. A fresh
`full` initialization explicitly publishes the bounded
publisher-authored acquisition and persistent-session baseline needed by the
two exact fixture profiles. Activate either profile with the same generic
operation:

```text
ryeos external-content activate config:ryeos-runtime/qwen3-0.6b-cpu-4096-activation online
ryeos external-content activate config:ryeos-runtime/qwen3-0.6b-cpu-2048-activation online
```

Activation submission returns a durable sync-job coordinate promptly. Observe
it with `service:sync/jobs/inspect`; once complete, repeating the command
returns the verified node-local receipt idempotently. Acquisition/import is
daemon-owned and restart-recoverable rather than tied to the CLI connection.

The node downloads exact immutable publisher archives into its private cache,
reproduces the worker's existing manifest pins, publishes ordinary consumer
bindings, and records target-local settlement. The online path needs no
installed assembler, public realization directory, named-root policy,
node-side build, or manual import/bind loop.

Reinstalling an existing node preserves its current node-owned policy
generation. Operators use stopped-node `ryeos node policy-apply` only when
deliberately changing that policy, adding an offline archive root, or repairing
an older/missing policy generation; an ordinary fresh full initialization does
not require hand-authored prerequisite policy files.

For a network-independent node, the same operation accepts an explicitly
selected node-policy archive root:

```text
ryeos external-content activate \
  config:ryeos-runtime/qwen3-0.6b-cpu-4096-activation \
  offline local-inference-archives
```

That root is removable/offline acquisition authority only. RyeOS verifies the
same signed archives and then uses its private digest-keyed cache; it is not a
lasting workload root, assembly directory, or manual binding path.

The two Qwen3-0.6B CPU routes share the same source and four immutable
realizations but own distinct signed worker contributors and persistent-session
identities through exact real-UID process ceilings of 4096 and 2048. They are
bounded recorded-class contract fixtures, not sealed qualification, and do not
define the future production model, device, context, trace, or training
architecture.
