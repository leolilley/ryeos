<!-- rye:signed:2026-05-24T12:10:39Z:dad01e11496a14b1835899901b92fbadcf4efb5f56369cc2af2a47acf4cb2703:XUm0XQodfxQT0_nqqQuyIVdxg749AgVkWmZyAfQjml0Shmb1XnszuVXIYLXh0Mp_q-UC_1iRYWfY76XgwV74CA:4b987fd4e40303ac -->
```yaml
category: ryeos/future
name: source-vs-composed-contracts
title: Source and Composed Descriptor Contracts
entry_type: implementation_guide
version: "1.0.0"
author: amp
created_at: 2026-05-25T00:00:00Z
description: Future implementation path for separating parser output guarantees, raw source descriptor contracts, composer input requirements, and final composed value contracts.
tags:
  - descriptor-validation
  - contracts
  - parser-output-schema
  - composer-input
  - boot-validation
```

# Source and Composed Descriptor Contracts

## Purpose

This note captures the advanced contract model that came up after descriptor instance validation landed.

The immediate fix should keep the current two-contract model:

- parser `output_schema` is a lower-bound guarantee about raw parser output;
- kind `composed_value_contract` is the final effective value contract;
- boot validates parser/kind compatibility, not full parser satisfaction;
- preflight and effective-item resolution validate actual descriptor instances.

That is enough for the current repo. The advanced path below should be implemented only if we need stronger authoring-time validation of raw, pre-composition descriptors.

## Current model

### Parser output schema

Parser descriptors declare `output_schema`.

For generic parsers such as `parser:ryeos/core/yaml/yaml`, this is intentionally broad:

```yaml
output_schema:
  root_type: mapping
  required: {}
```

That means the parser guarantees a YAML mapping root. It does not mean every YAML document contains fields like `launch`, `layout`, or `endpoint`.

### Composed value contract

Kind schemas declare `composed_value_contract`.

This describes the final value after item resolution and composition. For example, a composed surface must have `layout.root`, even if a child surface source file inherits `layout` from a parent.

### Runtime enforcement

Actual descriptor values are checked in two places:

1. Bundle preflight validates parsed descriptors only for identity-composed kinds, where parsed value equals composed value.
2. Effective item resolution validates every composed value after the composer runs.

This keeps generic parser declarations honest while still enforcing rich descriptor contracts on real instances.

## Problem the advanced path solves

The current model cannot express a distinct contract for raw source descriptors before composition.

Examples:

- A surface child source may omit `layout` because the extends-chain composer will inherit it, while the final composed surface must include `layout.root`.
- A directive source may allow shorthand fields that the composer normalizes into a stricter final shape.
- A future composer may require a raw field for composition but intentionally remove it from the final composed value.

Trying to force these semantics into either parser `output_schema` or `composed_value_contract` causes the wrong layer to own the wrong guarantee.

## Proposed advanced model

Keep all existing concepts, but add one explicit kind-side source contract.

```yaml
source_value_contract:
  root_type: mapping
  required: {}

composed_value_contract:
  root_type: mapping
  required:
    layout:
      type: single
      prim: mapping
      contract:
        root_type: mapping
        required:
          root: { type: single, prim: string }
```

The three layers become:

| Layer | Owner | Meaning | Validation seam |
|---|---|---|---|
| `parser.output_schema` | parser descriptor | Lower-bound syntactic output guarantee | Boot compatibility |
| `kind.source_value_contract` | kind schema | Valid raw parsed item before composition | Load/preflight before composer |
| `kind.composed_value_contract` | kind schema | Valid final effective item after composition | Post-composition |

## Boot validation semantics

Boot validation should not prove that a generic parser fully satisfies a kind's final contract.

With this advanced model, boot should check:

1. `parser.output_schema` is compatible with `kind.source_value_contract`.
2. The kind's declared composer handler accepts `composer_config`.
3. Optional: composer metadata declares that it can produce `kind.composed_value_contract` from `kind.source_value_contract`.

The compatibility check should remain a no-contradiction check:

- root types must not contradict;
- explicitly declared parser fields must not contradict source contract fields;
- missing source-required fields are not parser boot errors for generic parsers.

Actual required source fields should be enforced against concrete parsed values, not against generic parser declarations.

## Source validation seam

Add source validation inside the item load path after parser dispatch and metadata anchoring, before an item is promoted into the resolution pipeline.

Candidate seams:

- `resolution::context::load_item_at`, for root, ancestors, and references;
- `Engine::resolve`, if direct resolve should also enforce source contracts;
- bundle preflight for all source items where the kind declares `source_value_contract`.

The validation should report `InstanceValidationReport` just like composed value validation. Use a distinct error variant such as `SourceValueContractViolation` if callers need to distinguish pre-composition source failures from final composed failures.

## Composer input contracts

If source validation becomes composer-specific rather than kind-specific, introduce a composer input contract instead of overloading kind schemas:

```yaml
composer_input_contract:
  handler: handler:ryeos/core/extends-chain
  config_shape: ...
  source_value_contract: ...
```

Do this only if multiple composers serving the same kind need different source contracts. Until then, `kind.source_value_contract` is simpler and keeps authoring rules near the kind.

## Migration plan

1. Keep current boot parser compatibility check and post-composition validation.
2. Add optional `source_value_contract` to `KindSchema`, defaulting to an unconstrained mapping for mapping kinds.
3. Add schema-load validation for `source_value_contract` using the same `ValueShape` parser.
4. Add source instance validation in `load_item_at` and any direct resolve path that should reject malformed source values.
5. Add structured API/preflight reporting for `SourceValueContractViolation` if needed.
6. Migrate one high-value composed kind first, likely `surface`, where source and composed contracts naturally differ.
7. Re-sign bundles and run live bundle verification.

## Non-goals

- Do not make generic YAML parsers enumerate consumer-specific fields.
- Do not weaken strict `ValueShape::is_satisfied_by`; keep it for true shape-to-shape satisfaction.
- Do not make boot validation depend on live descriptor instances.
- Do not require source contracts for every kind before there is a demonstrated need.
- Do not expand preflight to composed kinds by guessing composer semantics.

## When to implement

Implement this advanced path only when one or more of these triggers appears:

- authoring needs clear pre-composition diagnostics for child descriptors;
- malformed composed-kind source files repeatedly fail too late in composition;
- multiple composers need different raw input contracts for the same final kind;
- tooling needs to validate raw descriptor files without resolving ancestors;
- the boot compatibility helper starts accumulating composer-specific exceptions.

Until then, the current middle path is preferred: parser output compatibility at boot, source/composer shape checks where already declared in composer config, and final composed value validation after composition.
