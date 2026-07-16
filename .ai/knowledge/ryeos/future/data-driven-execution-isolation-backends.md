---
category: ryeos/future
tags:
  - future
  - isolation
  - isolationing
  - node-policy
  - multi-platform
  - hosted-node
version: "0.1.0"
status: deferred
description: >
  Deferred architecture for typed, node-owned, data-driven execution
  isolation backends across operating systems and hosted workers.
---

# Data-driven execution isolation backends

## Status and reason for this note

Deferred. RyeOS does not currently need a portable isolation subsystem for its
primary trusted, single-user local-node use case. The Linux Bubblewrap path was
added during a hardening pass so the project could make a more honest distinction
between verified execution and OS confinement. It remains optional and disabled
by default.

The current implementation is useful groundwork, but it is not a backend-neutral
architecture. Its policy data is structured while its inspection, capture,
argument construction, filesystem model, and diagnostics are specifically built
around Bubblewrap and Linux. A future portability pass should preserve the
security properties that are real, remove the backend assumptions from the
engine contract, and avoid pretending unlike platform primitives provide the
same boundary.

This document is a design constraint for that later work, not a live schema or a
commitment to implement it now.

## Current boundary

Today:

- `mode: disabled` is the default and launches without an OS isolation;
- `mode: enforce` selects the one implemented backend, Bubblewrap on Linux;
- node policy is immutable for one daemon generation;
- items and requests cannot enable the backend or broaden its authority;
- exact backend and executable inspection is Bubblewrap-specific; and
- non-Linux enforced execution is unsupported.

Normal RyeOS verification, trust, authorization, capability, environment,
output-bound, process-attachment, and cancellation rules remain active when the
OS isolation is disabled. Those are separate properties and must stay separate in
the future model.

## Architectural rule

Execution isolation must be resolved as node-owned data and typed capabilities,
not as item-authored backend flags or scattered operating-system branches.

```text
node-owned policy + admitted execution requirements
  -> typed isolation requirements
  -> deterministic backend selection
  -> capability compatibility check
  -> backend-neutral isolation plan
  -> backend-specific prepared launch
  -> immutable execution + typed inspection/provenance
```

The engine decides what boundary is required. A backend adapter decides how to
realize that boundary on its platform. Neither the workload nor the adapter may
silently weaken the plan.

## Keep the concepts separate

Do not replace the current boolean with another broad `isolationed` flag. The
eventual contract must distinguish at least:

```rust
enum IsolationRequirement {
    Direct,
    ProcessConfinement,
    ResourceConfinement,
    HostileWorkload,
}

enum IsolationBackendKind {
    Direct,
    LinuxBubblewrap,
    LinuxWorker,
    MacOsWorker,
    WindowsAppContainer,
    WindowsWorker,
    RemoteWorker,
}

enum IsolationBackendStatus {
    Disabled,
    Available,
    Unavailable,
    Incompatible,
}
```

The final names may differ, but these states must remain enums in the contracts,
status surfaces, persisted records, and tests. Do not use free-form status
strings or infer strength from a backend name.

An isolation capability set should describe properties independently, for
example:

- filesystem visibility and write scoping;
- environment construction;
- host, isolated, or policy-filtered networking;
- PID and signal separation;
- open-file, process-count, CPU, memory, and I/O bounds;
- exact executable or image identity;
- workload-lifetime teardown;
- kernel-sharing or VM/worker separation; and
- supported observation and attestation evidence.

`ProcessConfinement` must not imply resource quotas. Resource quotas must not
imply a hostile-code kernel boundary. A remote or VM worker must not imply exact
entry-byte execution unless it can attest that property.

## Data ownership and narrowing

The node owner selects which backend implementations exist and which isolation
profiles they may satisfy. Workload metadata may declare requirements or narrow
an already enabled profile; it may never:

- enable isolation that node policy disabled;
- choose an executable or backend implementation;
- add filesystem, environment, network, secret, or callback authority;
- relax a node requirement or limit;
- request fallback to direct execution; or
- claim a stronger isolation class than the realized backend proves.

The effective plan is the intersection of:

1. immutable node policy;
2. deployment or principal policy when hosted execution exists;
3. signed runtime/tool restrictions;
4. caller/session delegation constraints; and
5. concrete launch context.

Every layer narrows. No layer broadens another.

## Future data shape

The eventual schema should use one current version with strict fields and typed
enums. Do not add compatibility aliases, legacy field names, or parallel v1/v2
documents while the feature remains unpublished. The following is illustrative,
not a live schema:

```yaml
version: 1
mode: disabled
backend:
  bundle: sandbox-linux-bubblewrap
  implementation: linux-bubblewrap
```

If profiles later become signed RyeOS items, node configuration should still
select and admit them. Merely placing an item in a project must not turn it into
host execution policy. Backend descriptors remain node-owned because they grant
authority to invoke host facilities.

Backend selection should be deterministic data, not `PATH` discovery or a
try-each-backend loop. An explicit ordered preference list is acceptable only
when every candidate proves the complete required capability set and the chosen
backend identity is recorded.

## Backend-neutral plan

The engine should compile policy and launch context into an `IsolationPlan`
without constructing Bubblewrap arguments or platform command lines. The plan
should carry typed operations such as:

- mount or present this opened authority read-only;
- present this opened authority writable;
- execute this verified artifact identity;
- construct exactly this environment;
- select the required network posture;
- apply these resource bounds;
- own this workload until authoritative teardown; and
- expose only these observation channels.

The adapter must either compile the complete plan or return a typed incompatibility
diagnostic. Partial enforcement is a launch refusal, not a warning followed by
execution.

A conceptual adapter boundary is:

```rust
trait IsolationBackend {
    fn kind(&self) -> IsolationBackendKind;
    fn inspect(&self) -> IsolationBackendInspection;
    fn capabilities(&self) -> IsolationCapabilities;
    fn prepare(&self, plan: &IsolationPlan) -> Result<PreparedIsolationLaunch>;
}
```

The prepared launch should retain the authority needed for execution rather than
re-resolving mutable paths. File-backed adapters can use descriptor-pinned or
privately captured artifacts. VM and remote-worker adapters will need equivalent
image, worker, protocol, and attestation identities instead of pretending they
have a local executable hash.

## Selection and failure semantics

Selection is resolved once for an immutable daemon policy generation or an
explicitly versioned hosted-job admission decision. Status and doctor surfaces
must report:

- requested isolation requirement;
- selected backend kind and stable identity;
- backend status as an enum;
- advertised and required capabilities;
- effective policy digest;
- inspection or attestation evidence; and
- the exact reason for an incompatibility.

Required isolation never falls back to direct execution. A node can run direct
only because policy explicitly selected `Direct` or because isolation is
explicitly disabled for that workload class. `Unavailable` and `Incompatible`
are failures when a stronger requirement was selected.

## Platform direction

Platform support must be claimed per tested backend and capability set, not from
the existence of similarly named OS features.

### Linux

The current Bubblewrap path can become the first process-confinement adapter.
Resource enforcement should be a separate cgroup v2 or worker-controller layer,
not invented Bubblewrap arguments or `RLIMIT_NPROC`. Hostile workloads still
need a dedicated worker, VM, or microVM selected by the deployment threat model.

### macOS

Do not port the Bubblewrap command model or claim support from an incidental
host isolation utility. A supported macOS path needs a maintained boundary with a
documented product contract and CI coverage. If no native primitive can prove
the required capability set, use a VM/dedicated worker backend or fail closed.

### Windows

A future adapter may compose restricted tokens, AppContainer, job objects, and
network policy where their combined semantics are sufficient. VM- or worker-grade
execution may use an appropriate Hyper-V-backed or dedicated worker boundary.
Each property must be advertised separately; the presence of AppContainer alone
must not be presented as equivalent to the Linux adapter or a VM.

### Other Unix systems

Direct execution is the only honest default until a maintained, tested adapter
exists. Source compatibility and generic process APIs do not establish a
supported isolation boundary.

### Remote and hosted workers

A remote worker is a valid backend shape once signed admission, authenticated
transport, durable jobs, principal-scoped authority, cancellation, and evidence
of the realized boundary exist. It should consume the same `IsolationPlan` and
return typed evidence, while the local node remains responsible for refusing a
worker that cannot satisfy the requested capabilities.

## Implementation sequence

Do not begin this work merely to make the current optional feature look more
general. Pull it forward when RyeOS supports another operating system, adds a
second real backend, or hosts code outside the node owner's trust boundary.

When triggered:

1. Extract a backend-neutral `IsolationPlan` and capability vocabulary from the
   current policy without changing behavior.
2. Move current Linux behavior behind a Bubblewrap adapter and prove exact
   parity with contract tests.
3. Replace Bubblewrap-shaped diagnostics with typed inspection, status, and
   provenance data.
4. Add resource control as a separate adapter/layer with authoritative
   workload-lifetime teardown.
5. Add an OS-specific or remote backend only with an explicit threat model,
   release packaging, and dedicated CI.
6. Add hosted hostile-workload profiles only when principal-scoped storage,
   secrets, networking, audit, quota, and cleanup exist around the worker.

No compatibility layer is required during this extraction. Keep one strict
schema and migrate the unpublished node-owned configuration directly.

## Required proof

Every backend needs shared contract tests plus backend-specific integration
tests in an environment capable of running it. The shared suite must prove:

- deterministic backend selection;
- strict enum and schema handling;
- no item-authored enablement or broadening;
- refusal on every missing required capability;
- no implicit fallback to direct execution;
- exact environment, filesystem, network, and resource-plan translation;
- immutable or attestable backend and workload identity;
- teardown on timeout, cancellation, output overflow, daemon/worker failure,
  and partial launch; and
- honest status/provenance for both success and refusal.

Symlink/race, inherited-file-descriptor, environment/network leakage, and
adversarial teardown cases belong in those dedicated backend suites. They
should not be simulated by unit tests that never invoke the platform boundary.

## Explicitly still deferred

- CPU, memory, process-count, and I/O quotas;
- VM- or microVM-grade isolation;
- kernel-exploit resistance;
- backend ownership and non-writability provenance beyond current capture;
- inherited-file-descriptor adversarial coverage;
- symlink and race-oriented isolation integration coverage;
- network and environment leakage integration coverage;
- a supported non-Linux node isolation;
- principal-specific hosted isolation and remote-worker attestation; and
- comprehensive plaintext zeroization above the crypto-key layer.

These are not properties of the current Bubblewrap implementation. They are
requirements to attach to the correct future backend or outer worker when a real
product/deployment trigger exists.
