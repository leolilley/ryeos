<!-- ryeos:signed:2026-08-27T04:20:18Z:3900703768eea447981501d07fec23b52ad8c786c224a94f598a0a16150050ed:O1Daa6KePacRWnZYasNo1awCdBkEofX3C+uHrnXEUP3e6f8jmdMkUpb5s2OPsJDRBGleITLPS0jDStFuebzcBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: local-inference
tags: [execution, external-content, persistent-session, local-model, replay]
version: "1.2.0"
description: >
  Operator activation, evidence, and recovery contract for admitted local
  model workers on ordinary or optionally isolated RyeOS nodes.
---

# Admitted local model workers

RyeOS runs a local model as admitted RyeOS content, not as an ambient HTTP
sidecar. A signed worker identifies the executable program and declares its
exact external realizations. The node imports those realizations into durable
content-addressed storage, binds them to the exact trusted consumer, and
launches the worker through the generic persistent-session contract.

The ordinary full source installation includes the concrete worker/provider
content:

```text
sudo scripts/pkg/install-local-direct.sh --populate --all --trust-source-publishers
```

That installation includes no isolation backend and does not import model
bytes. Neither is an error. Bubblewrap is optional node hardening, not a local-
inference activation prerequisite.

The shipped `worker:local-inference/local-tinygrad` and
`provider:local-tinygrad` are the current Qwen3-0.6B CPU acceptance fixture.
They are deliberately `recorded`, not `sealed`: no node qualification currently
proves a closed compiled-artifact/numerics set and two clean processes producing
the same canonical bytes.

## Exact execution boundary

The worker's publisher-authored source lives adjacent to its descriptor under
`.ai/workers/local-inference/lib/local-tinygrad/`. The signed descriptor binds
the source root, entry, and aggregate source-manifest digest. RyeOS admits that
source closure from the exact installed bundle generation.

The following are operator-imported external realizations:

- hermetic Python runtime and dynamic loader;
- exact tinygrad source tree;
- exact compiler/linker/toolchain closure; and
- model weights, tokenizer, configuration, template, and origin metadata.

When node isolation is disabled, RyeOS materializes the admitted source and
external realizations into a daemon-owned private workspace before process
start. The worker receives logical paths inside that private view, bounded
scratch/home/cache/tmp directories, and one daemon-owned session socket. It
does not execute from the assembly directory, live bundle source, a vendor
checkout, or a mutable model cache.

Disabled isolation is not OS confinement. A trusted signed process still has
the ambient host visibility allowed by the operating system. RyeOS therefore
reports isolation as disabled and makes no claim that filesystem or network
access was kernel-blocked. Signature, trust, capability, exact-input,
attachment-before-execution, process ownership, resource, cancellation,
observation, recording, and replay checks remain active.

With an enforced compatible isolation backend, the same worker may additionally
receive read-only mounts, private scratch, and isolated networking. That
stronger launch provenance does not change recorded provider semantics or make
the backend a dependency of the route.

## Node policy

Persistent sessions are unavailable when their node-owned policy is absent.
External import is unavailable without a node-owned named-root policy. Both
policies are validated and node-signed through their registered node-config
sections:

- `<system>/.ai/node/persistent_sessions/policy.yaml`
- `<system>/.ai/node/external_content/policy.yaml`

The external root identity is not a path string alone. Open the intended
assembly directory and record its current device and root inode:

```text
stat -c '%d %i' /absolute/operator-owned/assembly
```

Example source policy:

```yaml
schema: 1
roots:
  local-model-assembly:
    path: /absolute/operator-owned/assembly
    containing_device: 66306
    root_inode: 1745406
limits:
  max_depth: 64
  max_entries: 10000
  max_file_bytes: 2147483648
  max_total_bytes: 2147483648
  store_budget_bytes: 4294967296
  minimum_free_bytes: 4294967296
```

The numbers are examples except where the publisher-signed activation fixture
requires a minimum bound. Measure the actual root identity; never copy the
example device or inode.

Example persistent-session source policy:

```yaml
schema: 1
limits:
  max_pool_groups: 4
  max_total_processes: 1
  max_total_address_space_bytes: 17179869184
  max_total_cpu_seconds: 3600
  max_open_streams: 8
  max_active_streams: 1
  max_active_streams_per_subject: 1
  max_stream_backlog_bytes: 16777216
  max_total_backlog_bytes: 16777216
```

The aggregate address-space and CPU ceilings must cover the worker lifecycle
reservation. A smaller pool is refused before spawn.

Author policy sources outside the live `.ai/node` namespace and apply them
through the typed node-policy command:

```text
ryeos node policy-apply external_content /path/to/external-content-policy.yaml
ryeos node policy-apply persistent_sessions /path/to/persistent-session-policy.yaml
```

Follow the command's maintenance requirement for the installed generation. Do
not edit or separately sign live node-policy files. Absence or invalid policy
is a refusal, not permission to use compiled defaults.

## Optional isolation hardening

An operator who needs per-process OS confinement may separately install a
compatible isolation backend and apply an enforced isolation policy. The
backend bundle, build, installation, and node policy belong to
`knowledge:ryeos/core/node/execution-isolation`; they are not repeated here.

For this worker, an enforced launch should narrow the process to its admitted
source, external realizations, daemon-owned scratch, target channel, declared
environment, and isolated network. Doctor must report the backend and required
capabilities as available before such a launch.

The ordinary acceptance in this document deliberately runs without that
backend. A second optional acceptance may prove enforced-path parity and record
the stronger provenance.

## Exact acceptance realization

Construct the four operator-owned fixture inputs from exact upstream archives
into a new assembly directory. The helper is authoring/activation tooling, not
worker content. It verifies every downloaded artifact and refuses an existing
output path:

```text
python3 bundles/local-inference/assemble.py \
  --cache /absolute/operator-owned/download-cache \
  --output /absolute/operator-owned/assembly
```

`--offline` forbids downloads when the exact cache is already populated. The
helper does not mint RyeOS identity. The import service constructs canonical
manifests, and the publisher-signed fixture is the authority for their expected
hashes.

`config:ryeos-runtime/local-tinygrad-activation` contains the path, storage
tier, shape, bound, and expected manifest hash for each component. Import each
component as the configured local operator, for example:

```text
ryeos execute service:external-content/import --no-stream \
  '{"root":"local-model-assembly","path":"runtime","shape":"tree","storage":"content","maximum_bytes":104857600}'
```

Repeat with the fixture's exact values. Compare every returned `manifest_hash`
with `expected_manifest_hash`; any difference stops activation. Tree imports
intentionally use the canonical manifest hash rather than an author-supplied
single-file digest.

Each import returns `staging_id`, `request_digest`, and `manifest_hash`. Bind
that exact tuple before staging authority expires:

```text
ryeos execute service:external-content/bind --no-stream \
  '{"staging_id":"<from-import>","request_digest":"<from-import>","manifest_hash":"<from-import>","consumer_ref":"worker:local-inference/local-tinygrad"}'
```

The bind must report the installed bundle publisher and exact consumer. Knowing
a manifest hash is not mount authority; project content, another publisher, or
another consumer cannot reuse the binding implicitly.

## Activation sequence

1. Assemble the exact fixture inputs and measure the assembly root identity.
2. Install the ordinary full current RyeOS bundle set.
3. Apply external-content and persistent-session policies through the typed
   node-policy command.
4. Run `ryeos node doctor`. Require initialized/trusted bundle state, adequate
   storage, and valid node policies. No isolation backend is expected for the
   primary acceptance.
5. Start the daemon if the policy command required maintenance/offline mode.
6. Import and bind the four exact realizations.
7. Validate the smoke directive/provider/worker without execution and require
   every expected/observed realization digest to match.
8. Run the first-bank and restart/replay acceptance below.

Do not perform historical schema resets from an old version of this document.
Clean-cut retirement is generation-specific: run only the exact reset command
named by current startup or snapshot-status evidence, inspect its dry run, and
follow the current node-operation knowledge. Activation is not general
permission to retire thread history, replay indexes, bindings, or node state.

## Bank and replay acceptance

Execute:

```text
ryeos execute directive:local-inference/examples/tinygrad_smoke --no-stream '{}'
```

The first run must:

- resolve `provider:local-tinygrad` and the exact admitted worker;
- execute under disabled isolation from the daemon-owned private realization
  view;
- settle an `ExplicitlyFree` attempt at exactly zero;
- retain the daemon-owned terminal observation;
- publish or fold one provider-call record; and
- confirm publication proof before the directive succeeds.

Stop and restart the daemon, then execute the identical directive again. The
second run must return from `effect_record` with publication
`not_applicable`, no worker lease/contact, no model execution, and no new
reservation.

Changing the worker source, runtime, tinygrad tree, toolchain, model,
tokenizer/template, provider config, request, sampler, admitted execution
realization, or relevant authority moves the coordinate. Missing, corrupt, or
contradictory indexed evidence fails closed; it never falls back to a model
call under the old identity.

## Current scope and future profiles

The fixture uses CPU, Qwen3-0.6B, 2,048 tokens of context, 256 output tokens,
one active stream, and disabled mutable compiler caches. It is the contract
probe for model mapping, tokenizer/template behavior, sampling, streaming,
cancellation, persistent lifecycle, recording, and replay.

A serious model/device route must use a separate signed profile that binds its
exact model/tokenizer/runtime/compiler content, backend/device requirements,
numerics, quantization, context, resources, sampler, and trace policy. It must
not mutate this fixture or auto-select ambient hardware. Qualification,
generation-state capsules, traces, distillation, training, and offline export
are owned by the linked future local-execution knowledge until implemented.
