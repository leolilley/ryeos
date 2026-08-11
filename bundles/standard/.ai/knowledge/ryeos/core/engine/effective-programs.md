<!-- ryeos:signed:2026-08-11T02:28:29Z:83ea8d77db7a468bf5aeac253383dd7919204b0c5f42ca73a0d328f667fcc206:1bGq9gvAee7tql6zYPFop8lgl0mO3G3/iXuKAxMH/4t3BTARx4OxZ1K5xhyyWzzL9dhB6wTXfTUJUQospFLODA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/engine
tags: [engine, composition, identity, hooks, recovery, field]
version: "1.0.0"
description: Finalized effective programs, captured hook policy, executable identity, and exact recovery.
---

# Effective programs

Invariant: a managed runtime executes one finalized effective program. It does
not execute a root document plus implicit inherited or live configuration.

## From source to launch authority

The admission order is fixed:

1. resolve and verify the root, ancestors, and declared references;
2. compose the kind's effective value using its signed field rules;
3. run every kind-declared launch augmentation;
4. capture authored and signed configured hook policy;
5. run the kind-declared semantic validator over the complete value;
6. revalidate every mutable capture dependency;
7. compute `effective_definition_digest` once;
8. seal the exact program and only then mint callback authority and spawn.

The engine exposes an opaque validated candidate and a candidate-bound
authority proof. Only their checked consumption can construct
`FinalizedEffectiveProgram`. Launch envelopes and sealed requests accept that
finalized type, not a mutable `ResolutionOutput`.

For kinds with `resolve_extends_chain`,
`resolution.composed.composed` is executable. `root.raw_content` and the
contributors' raw bytes are retained evidence. A runtime must not reopen paths,
re-resolve ancestors, or reconstruct behavior from the root bytes.

## Identity vocabulary

The identity fields are deliberately separate:

| Field | Meaning |
|---|---|
| `root_raw_content_digest` | signature-stripped root-source bytes |
| `source_content_digest` | complete signed source bytes, including its envelope |
| `effective_definition_digest` | exact effective resolution, composed/derived value, policy facts, and authority provenance |
| `admitted_launch_capsule_hash` | exact sealed program plus runtime/executor closure and invocation authority |

`effective_definition_digest` is canonical SHA-256 over a versioned seed. It
includes the root, ordered ancestors, canonically sorted referenced items and
reference edges, source spaces, trust classes, signer fingerprints, the
effective trust fold, and the complete `KindComposedView`. Paths, timestamps,
resolver diagnostics, invocation inputs, raw payload bytes, and whole signature
envelopes are excluded.

The full captured hook plan is part of the composed view. Changing operator or
project hook policy therefore creates a new effective version for subsequent
launches. The canonical item ref is the conceptual family used for comparing
runs; the effective digest is the exact executable version. No weaker program
hash is an execution join key.

## Captured hook policy

Hook capability is declared by the owning kind. The signed declaration names
the authored path, effective-plan derived key, events, context roots, and
allowed result modes. Graph and directive event lists are not hard-coded in the
launcher or callback service.

The captured `ryeos.hooks.effective.v1` plan contains authored, builtin,
infrastructure, context, operator, and project layers. Each layer carries its
normalized definitions and exact dispatch grants. Configured policy comes from
strictly signed config items:

| Layer | Config identity | Required authority |
|---|---|---|
| builtin, infrastructure, context | `config:ryeos-runtime/hooks/base` | trusted bundle |
| operator | `config:ryeos-runtime/hooks/operator` | trusted node |
| project | `config:ryeos-runtime/hooks/project` | trusted project |

Configured definitions use `target: {kind, event}`. Capture validates every
target against the installed signed kind contracts and projects only the
current owner kind into its plan. Unknown target pairs, duplicate effective
IDs, malformed or reserved grants, wrong source authority, invalid context
references, and disallowed layer/result combinations fail admission.

Before any callback token, capsule, or runtime process exists, the executor
compiles the captured plan with the runtime's single condition/template
compiler and decodes each action with the runtime's single action parser.
There is no admission-only approximation of either grammar. Configured actions
must dispatch inline, and every action target and reference binding must be
covered by that layer's source-owned grants. A literal canonical target needs
exact coverage; a target template with a fixed canonical kind needs kind-wide
coverage; a template that can vary the kind needs execute authority across
kinds. Ambiguous templates are never admitted on the promise that one rendered
value might happen to be allowed.

Authored hooks use the launching program's admitted caps. Configured hooks use
only their source layer's grants. Infrastructure is observer-only; graph hooks
are observer-only at every layer. Directive authored, builtin, context,
operator, and project hooks may use control only where the signed event permits
it.

The launcher derives exact callback authorizations from the captured plan. The
runtime compiles that same plan. Recovery reads it from the capsule. No runtime
loads hook configuration from the filesystem and no callback trusts a runtime
to report its own authority.

Callback preflight matches owner kind, event, ID, layer, result mode, context
contract, canonical definition ref, root raw-content digest, and effective
digest before ledger reservation. The dispatch key commits to the same identity
and source grants. Runtime-authored hook evidence event kinds are refused on the
ordinary append path; only the daemon's completed-hook outcome path may publish
them.

## Recovery

Recovery begins with the admitted capsule, not current item resolution. The
daemon verifies the capsule, decodes the full sealed request, proves its
invocation-stripped program equals `exact_program`, rechecks current signer
revocation for every definition and configured-policy contributor, recomputes
the effective digest, and compares checkpoint and trace identity before spawn.

Live source or config changes affect future launches only. They never mutate an
admitted run. Current revocation policy still applies, so exact bytes are not a
way to continue trusting a revoked signer.

## Current and admitted field projection

The project field obtains a current graph through the executor's read-only
effective-program projection. It uses the same resolve, compose, capture,
validate, proof, and digest path as fresh launch while minting no thread, token,
capsule, or runtime. A kind with launch-only augmentation and no safe projection
contract is refused rather than represented incompletely.

Admitted field facts are decoded from the sealed capsule. Current and admitted
definitions share the same effective-definition core and topology builder.
They join only when their effective digests match. Source-version and
policy-source entities retain the separate source digests, signers, spaces,
trust, layer, and contribution role so an operator can see why two effective
versions differ.

## Standing disciplines

1. **Admission caching keys on digests only.** Any memoization of
   resolve/compose/capture/finalize work must key on the content digests of
   its inputs (root, ancestors, config snapshots). A cache keyed on paths,
   mtimes, or session state reintroduces ambient behavior through the cache.
2. **Epochs are rare, boring, and about identity.** The schema-epoch
   mechanism exists for identity cuts, not migration convenience. After the
   effective-definition activation the identity layer is expected to go
   quiet; each further epoch must name what identity changed.
3. **Occurrence coordinates are runtime-asserted by acceptance.** The daemon
   bounds and shape-checks hook occurrence coordinates but does not validate
   per-event coordinate presence or values; occurrence identity is trusted to
   the admitted runtime, and at-most-once is per asserted occurrence.
   Malformed coordinates degrade in projection rather than erroring history.
4. **Composer binaries are a trust boundary.** `requires` narrowing is
   enforced by the trusted-bundle composer with the engine's post-compose
   defense and validator cap-parity as backstops; the shared
   `capability_cover` module is the single coverage semantics for both.
