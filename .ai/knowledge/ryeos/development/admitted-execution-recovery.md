<!-- ryeos:signed:2026-07-27T22:47:55Z:a607f143b55e4c4ee6d1dfe5826ed003d20b539611a711939eede04f5aed1af1:iXomXDd/ot/af1s88pMCXp4Z2NhZjqAj64SZSfiHuQO6GEpvqhb+jecrmXGnRqsx3O6+6OB0dYIcMJUVPR+CAg==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
```yaml
category: "ryeos/development"
name: "admitted-execution-recovery"
title: "Admitted Execution Closure & Recovery"
description: "How a managed launch is sealed at first admission into its signed capsule and recovered/relocated verbatim, never re-derived from mutable registries"
entry_type: reference
version: "1.0.0"
```

# Admitted Execution Closure & Recovery

A managed execution's *first admission* is a content-pinned decision: which exact
program, runtime, executor, protocol, and project authority this thread runs
under. When the daemon later recovers that execution (after a restart, or to
relocate it for spawn), it must reproduce that decision **exactly** — never
re-derive it from runtime, protocol, executor, or kind registries that may have
changed since. The **admitted execution closure** is how the decision is sealed
so recovery can consume it verbatim.

This is "execution is an object" applied to recovery: admission produces a
durable, signed object; recovery *extends* that object rather than recomputing
it. It is the recovery-side inverse of the forward admission path (see
`architecture.md` for the admission flow, and the resolution cache below).

## What is sealed

`AdmittedExecutionClosure`
(`crates/state/ryeos-state/src/objects/admitted_launch_capsule.rs`) is the
exact, **secret-free** execution material captured at admission and stored in the
thread's signed `AdmittedLaunchCapsule`. Two shapes:

- **`ManagedRuntime`** (a graph/directive/knowledge runtime): the exact
  `prepared_runtime_launch`, the signed `runtime_descriptor_document`, the signed
  `protocol_descriptor_document`, and the `executor_blob_hash` (content hash of
  the executor). Everything needed to re-spawn the runtime with the same code and
  protocol.
- **`DirectItemExecutor`** (a direct subprocess/tool): the exact
  `execution_plan`, the signed `protocol_descriptor_document`, the
  `admitted_project_root`, and the `command`, which is one of:
  - **`ContentAddressed { executable_blob_hash }`** — the command binary is a
    content-addressed CAS blob. Recoverable: the blob is reproducible from its
    hash.
  - **`NodePolicy`** — a node-policy command. **Not restart-recoverable** by
    design; recovery of a node-policy direct execution fails closed rather than
    resurrecting a command whose bytes were never content-pinned.

Everything sealed is either a content hash or a signed descriptor document —
sealed material is pinned, not a pointer into mutable space.

## What recovery must NOT do

> Recovery may apply *stricter* current trust and isolation policy, but it must
> never rebuild the sealed values from mutable runtime, protocol, executor, or
> kind registries.

The asymmetry is the whole point: **the *what* is immutable; the *gate* may only
tighten.** A restart that runs under a narrower isolation policy or a revoked
signer set is allowed to *refuse* recovery, but never to re-resolve a *different*
program for the same admitted thread. Recovery reads the exact program and
closure; it never asks project or bundle space to recreate an earlier admission.

## Recovery re-validation

`SealedRootExecutionRequest::restore_from_admitted_capsule`
(`crates/daemon/ryeos-app/src/thread_lifecycle.rs`) does not trust the sealed
blob blindly. It decodes the capsule's `sealed_invocation` and cross-checks it
against the capsule's *independently rooted* fields, failing closed on any
divergence:

- sealed program value == `capsule.exact_program`
- sealed program hash == `capsule.exact_program_hash`
- sealed project authority == `capsule.project_authority`
- sealed runtime ref == `capsule.runtime_ref`
- sealed executor ref == `capsule.executor_ref`

A mismatch bails with *"admitted capsule invocation contradicts its rooted
program authority."* Only a self-consistent capsule is restored, via
`restore_for_reconstructed_provenance`. `recover_from_execution_closure`
(the `PreparedItemPlan` side) additionally refuses a closure whose driver/artifact
identity does not match the recovery it was asked for, and refuses `NodePolicy`
direct commands.

## Relocation

A recovered direct plan can be **relocated** for spawn (a different working
materialization, e.g. a fresh project root) via `relocate_admitted_direct_plan` /
`validate_direct_plan_portability` / `relocate_project_for_spawn`. Portability is
validated before the move, and the **project authority is not permitted to change
during recovery** — relocation moves *where* the sealed plan runs, never *what*
authority it runs under. Runtime parameters carry typed stdin so a relocated
launch feeds the same inputs across the boundary.

## Relationship to the resolution cache

The resolution cache (`crates/daemon/ryeos-app/src/resolution_cache.rs`) and the
admitted closure are complementary, and both encode the same rule — *never
re-derive an admission from mutable state* — for two different phases:

- The **cache** accelerates the *forward* admission path: it memoizes the
  resolve/compose pipeline and serves a hit only after content revalidation
  proves nothing changed.
- The **closure** is the *recovery* source of truth: it seals admission's output
  so a crashed launch is restored from exact material.

Recovery consumes the sealed closure and **does not re-enter admission** — so it
never consults the resolution cache. There is one source of truth per phase, no
overlap.

## See also

- `execution-is-an-object` and `portable-execution-white-paper-thesis` (papers) —
  the durability/portability thesis this implements.
- `../standard/runtimes/graph-runtime.md` — the forward execution-model and
  durability contract (checkpoints, continuation, the commit fence).
- `filesystem-durability.md` — the durable-write primitives the capsule relies on.
