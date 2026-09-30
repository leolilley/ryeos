<!-- ryeos:signed:2026-09-30T01:24:31Z:898a26aab1b26321980e16909379fbe7692ac5f490c341dffb48b7260f6fdfb2:A7p6zZqjWN9kSLKhN79yWvBy9rBfPlVTQ6qCX32mpTXcXWt/F/fj6eqL+SGL7mDVP9DvROiATxpass4MK76wDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "admitted-execution-recovery"
title: "Admitted Execution Closure & Recovery"
description: "How a managed launch is sealed at first admission into its signed capsule and recovered/relocated verbatim, never re-derived from mutable registries"
entry_type: reference
version: "1.1.1"
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

  Its artifact identity separately binds the caller-named root subject's whole
  signed-source digest, signer, and source-generation identity. Those are root
  authority—not the declared executor hop. The declared `executor_ref` is
  checked against the first hop after `execution_plan.root_ref`, while the
  terminal executor remains `execution_plan.root_executor_id`.

External material is sealed by content hash or signed descriptor identity.
Exact embedded values are covered by the signed capsule and cross-checked
against its independently rooted authority; none is a pointer into mutable
registry or project space.

The sealed invocation also carries the complete typed executor route selected
at admission (including the managed runtime ref/hash/signer/served kind, or the
exact direct route). A discriminant or executor string alone is insufficient:
recovery must be able to validate the route without consulting today's runtime
registry.

## Product qualification admission and consumption

Product qualification retains its daemon-derived purpose and the complete
canonical Root product selection batch. Fresh admission and sealed-request
validation apply the same subject rule: the exact policy subject is unqualified
and cannot be replaced by an auxiliary. Read-only pinned qualification may carry
additional inputs authenticated through normal product composition. It requires
the exact snapshot authority and no workspace outputs. The existing projectless
fixed-pin lane remains supported; it cannot acquire auxiliary inputs implicitly.

Direct verifier compilation uses `ADMITTED_DIRECT_PROJECT_ROOT` for logical
paths. The physical materialization and its lifetime proof remain in the exact
pinned admission context; a temporary checkout path is not a portable program
identity. The same snapshot resolver is used when a dependency's qualification
requires reopening its own retained pinned context.

Consuming a qualification additionally reconstructs the checker under current
trusted policy and compares its exact identity with the retained evidence. This
eligibility check does not change a recovering invocation's sealed program or
authorize execution with newly selected inputs. See
[remote development and qualification](remote-development-and-qualification.md)
for the supported signed profiles and input contract. Source coverage of these
boundaries is separate from an installed verifier's execution and settlement
evidence.

## Qualification schema cut

The current qualification purpose is a tagged source: a captured product and
an activated content subject have different provenance. Sealed-root schema 22
and admitted capsule schema 31 retain that distinction. The current decoder
does not reinterpret predecessor schema-21 roots or schema-30 capsules as new
launch authority. A node retaining those heads must complete an explicit,
reviewed execution-history recovery cut before the new generation can admit
fresh execution. Retained CAS bytes can remain archival evidence; they do not
become current qualification or consumer permission.

Dispatch-effect replay also moves to operational epoch 12 for the schema-31
contract. Its supported explicit reset retires predecessor dispatch-effect
rows and preserves independent provider-call rows from the immediate prior
epoch. This source decision does not reset a node. Recovery must use the exact
final binary's guarded dry-run and account for every authority it would retire;
affected products and consumer proofs require current qualification afterward.

## What recovery must NOT do

> Recovery may apply *stricter* current trust and isolation policy, but it must
> never rebuild the sealed values from mutable runtime, protocol, executor, or
> kind registries.

The asymmetry is the whole point: **the *what* is immutable; the *gate* may only
tighten.** A restart that runs under a narrower isolation policy or a revoked
signer set is allowed to *refuse* recovery, but never to re-resolve a *different*
program for the same admitted thread. Recovery reads the exact program, prepared
launch, capability closure, executor route, and execution closure. Its current
policy view loads trust/revocation only; it does not rebuild parser, kind,
runtime, launch-preparer, capability-manifest, or executor registries.

## Recovery re-validation

`SealedRootExecutionRequest::restore_from_admitted_capsule`
(`crates/daemon/ryeos-app/src/thread_lifecycle/sealed_request.rs`) does not
trust the sealed blob blindly. It decodes the capsule's `sealed_invocation` and
cross-checks it against the capsule's *independently rooted* fields, failing
closed on any divergence:

- sealed program value == `capsule.exact_program`
- sealed program hash == `capsule.exact_program_hash`
- sealed project authority == `capsule.project_authority`
- sealed runtime ref == `capsule.runtime_ref`
- sealed executor ref == `capsule.executor_ref`
- sealed typed executor route == `capsule.artifact_identity`

The capsule's `effective_caps` are also the exact admitted capability closure.
Recovery may validate an additional captured equality assertion, but it never
re-mints runtime-manifest capabilities or re-derives composed grants.

A mismatch bails with *"admitted capsule invocation contradicts its rooted
program authority."* Only a self-consistent capsule is restored, via
`restore_for_reconstructed_provenance`. `recover_from_execution_closure`
(the `PreparedItemPlan` side in
`crates/daemon/ryeos-app/src/thread_lifecycle/direct_execution.rs`) additionally
refuses a closure whose driver/artifact identity does not match the recovery it
was asked for, and refuses `NodePolicy` direct commands.

## Relocation

A recovered direct plan can be **relocated** for spawn (a different working
materialization, e.g. a fresh project root) via `relocate_admitted_direct_plan` /
`validate_direct_plan_portability` / `relocate_project_for_spawn`, colocated in
`thread_lifecycle/direct_execution.rs`. Portability is validated before the
move, and the **project authority of one admitted continuation segment is not
permitted to change during recovery** — relocation moves *where* that segment
runs, never *what* authority it runs under. A COW continuation that advances
generation receives a new segment-specific capsule rooted in the new operational
authority while retaining its stable base lineage.

The retained `subject.source` file lives under a private capsule directory, but
that directory is never project authority. Projectless recovery keeps
`materialized_project_root = None`; live/pinned/COW recovery rebinds both the
request subject and admitted subject to the exact reconstructed provenance
workspace. Only `source_path` points into the capsule.

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
