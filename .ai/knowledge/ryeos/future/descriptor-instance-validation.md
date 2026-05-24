<!-- rye:signed:2026-05-24T02:17:39Z:a5c3d88232614c8d6b5b82ea215312ab0a422d68012247d2e76664ae0570562f:-0nl43OB5Kji17d5NnFkHI9w9vwQZvNr3ZS6yQN2zYXI4iHVxRn4fASptfGOdciNA8Fvu0XJK-J16y3WaTgPAQ:4b987fd4e40303ac -->
```yaml
category: ryeos/future
name: descriptor-instance-validation
title: Descriptor-Instance Validation Advanced Path
entry_type: implementation_guide
version: "1.0.0"
author: amp
created_at: 2026-05-24T00:00:00Z
description: Future implementation path for extending kind schemas with nested-mapping shapes, enum/const value constraints, and engine-side validation during effective-item resolution and bundle-verify; allows descriptor authors to declare value-level constraints that today have to be re-enforced by every binary that reads the descriptor.
tags:
  - kind-schemas
  - composed-value-contract
  - effective-item
  - bundle-verify
  - descriptor-validation
  - rye-native
```

# Descriptor-Instance Validation Advanced Path

## Purpose

This note captures the optional advanced path for descriptor-instance
validation. It is the right home for "value-shape enforcement that
today leaks into Rust binaries because the kind-schema contract is
too shallow to express it."

It is **not** required by the
client/surface/renderer corrective implementation
(`.tmp/client-surface-substrate-implementation/00-corrective-plan-v2.md`).
That plan resolved the immediate concern (Revision B) by removing
cross-bundle gating from `ryeos-core-tools` entirely — core-tools
stopped modelling `client.serves` so the missing validator didn't
matter at the binary boundary. This entry is the deferred *system*-level
improvement that still wants doing.

## Why we want it (independent of `client.serves`)

1. **Catch typos at publish, not at user-exec time.** A typo such as
   `launch.mode: ladnch_browser` slips through current `bundle-verify`
   because the kind schema can only say `launch` is a required mapping;
   it cannot say `launch.mode` must be one of `{"cli_exec",
   "daemon_ui"}`. Only the consuming binary catches the typo, and only
   when a user runs the descriptor.
2. **Kind authors get a real API contract.** Today writing a new kind
   means writing the YAML, writing a Rust reader, and hoping nobody
   mis-fills the descriptor. Rich contracts let the kind author
   declare the shape once and trust the engine to enforce it
   everywhere the descriptor is consumed.
3. **Collapse a whole category of "the binary validates for me" code.**
   Wherever a handler or launcher parses a descriptor and gates on
   string values, that gate is doing work the engine should own. With
   rich contracts those gates move to schema declarations.
4. **Future kinds get cheaper.** Adding `kind: graph` or
   `kind: dashboard` becomes ship-a-schema with no Rust-validator
   follow-up.
5. **Tooling foundation.** Machine-readable contracts unlock
   generated reference docs, IDE completion for descriptor authoring,
   and kind-author lints. Today every author needs to read both the
   schema YAML and the consuming Rust to know what's valid.

## Current state

Kind schemas declare *shape* through
`composed_value_contract` / `ValueShape` in the engine. As of
`next` HEAD that contract is shallow:

- root type (mapping, sequence, scalar)
- required vs. optional top-level fields
- primitive kinds per field (`string`, `mapping`, `array`, etc.)
- unions of primitive kinds

It does **not** support:

- nested mapping shapes — `serves: { kind: string, renderer: string }`
- enum / const value constraints — `kind ∈ {"surface"}`
- typed sequence elements — `args: [{ name: string, flag: string }]`
- conditional shapes — "if `launch.mode == cli_exec` then
  `launch.binary_ref` is required"

Validation runs at descriptor parse time (shape check) and at
`bundle-verify` for signature/structure. Value-level checks live in
whichever Rust binary reads the descriptor — and after the corrective
plan's Revision B, are explicitly *not* expected in core-tools.

## What this work would deliver

### Scope (minimum)

1. Extend the contract DSL to express:
   - **Nested mapping shapes.** A field of type `mapping` may carry
     a contract for its own sub-fields.
   - **Enum / const constraints.** A scalar field may be constrained
     to a closed set of values.
   - **Optional unions.** A field may be `T | null` (or `oneOf`).
2. Add a typed engine error:
   `EffectiveItemContractViolation { path, expected, found }`.
   Path is dotted (`launch.mode`, `serves.kind`, etc.) so messages
   point at the failing field.
3. Run the validator inside `Engine::effective_item` after
   composition. The shape check today runs at parse time; this is the
   *post-composition* check so composer-introduced shape changes are
   caught.
4. Run the same validator inside `bundle-verify` so structural
   problems are caught at publish, not at exec.
5. Migrate every kind schema in `bundles/core` and `bundles/standard`
   to use the richer contracts. Notably:
   - `client.launch.mode ∈ {"cli_exec", "daemon_ui"}`
   - `client.serves.kind ∈ {"surface"}` (extensible by kind authors)
   - `surface.affordances[*]` element shape
   - `service.handler` constraints
   - any other place currently gated by a Rust string compare.
6. Tests: per-contract-feature unit tests + per-kind round-trip tests
   proving a deliberately malformed descriptor is rejected by both
   `effective_item` and `bundle-verify`.

### Out of scope (for this slice)

- Conditional contracts (`if X then Y required`). Add later if
  evidence justifies it.
- JSON Schema interop. Rye contracts stay Rye-native.
- IDE completion / generated docs. Tooling that consumes the richer
  contract is separate downstream work.

## When to implement this

Implement when **any** of the following appears:

- Two or more binaries are found duplicating value-level descriptor
  validation that the schema cannot express.
- A new kind is being designed and the author wants enum/nested
  constraints in the schema rather than the consuming binary.
- A descriptor typo escapes `bundle-verify` and bites in production
  (or in a user demo).
- Cross-bundle gating temptations re-emerge: someone starts adding
  Rust value-gate code to a binary that doesn't own the kind whose
  descriptor it's reading.

## When *not* to implement this

- "Looks tidier" alone is not a trigger. The corrective plan
  removed the only immediate pain point.
- Before the corrective train has shipped. The descriptor surface
  needs to settle before designing the richer contract DSL; designing
  in a vacuum risks paint-yourself-into-a-corner choices.

## Sizing

L/XL relative to a corrective-plan slice. The DSL design itself is a
slice; the engine integration is a slice; the bundle migration is a
slice; the bundle-verify integration is a slice. Plan for at least
three short slices rather than one large one.

The corrective plan's process rules (slice-by-slice, tests first,
human review between slices, no autonomous runs) apply unchanged.

## Trigger checklist before starting

1. The client/surface/renderer corrective plan has shipped.
2. At least one of the "when to implement" triggers has fired.
3. A short design note exists for the contract DSL syntax (oracle
   round recommended) covering nested mappings, enums, and unions
   without precluding future conditional contracts.
4. A migration audit exists listing every current kind schema and
   the contract upgrade it will receive — including the kind schemas
   shipped by any new bundles since the corrective plan landed.

## Related

- `.tmp/client-surface-substrate-implementation/00-corrective-plan-v2.md`
  — the corrective plan that explicitly defers this work (see "What
  this plan deliberately does NOT do").
- `crates/core/engine/src/...` — `ValueShape` /
  `composed_value_contract` is where the DSL extension lands.
- `crates/tools/core-tools/src/bin/ryeos-core-tools.rs` — the
  binary that motivated Revision B; after the corrective plan it no
  longer models `client.serves`, removing the immediate need.
- Bundle-verify implementation (in `ryeos-core-tools`) — gains the
  validator pass.
