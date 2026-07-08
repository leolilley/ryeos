```yaml
category: ryeos/future
name: per-row-affordance-gating
title: Per-Row Affordance Gating
entry_type: implementation_guide
version: "0.1.0"
description: Gate a list view's affordances per row on record facts (a `when:` predicate), so conditional actions render only where they mean something.
tags:
  - ryeos-ui
  - views
  - affordances
```

# Future: Per-Row Affordance Gating

## Status

Deferred. List-view affordances are declared per VIEW and rendered for
every row; a row where the affordance is meaningless simply no-ops on
activation. Cosmetically fine at today's affordance counts; this doc
records the vocabulary to add when it stops being fine.

## The live example

The threads list declares a "Watch child" affordance that aims the route
at a suspended parent's followed child chain
(`{record.follow.child_thread_id}`). On non-follow rows the follow fact is
absent, both merge fields resolve to null, and activation does nothing.
The deferral is noted inline at that affordance in
`bundles/ryeos-ui/.ai/views/ryeos/threads/list.yaml`.

## Design

An affordance-level `when:` predicate over record fields — presence and
equality only, reusing the projection vocabulary the tone maps already
use (`{field, map}` shapes), NOT an expression language:

```yaml
affordances:
  - id: watch-child
    when: { field: follow.role, equals: suspended_parent }
    ...
```

Evaluate at view-model projection in `crates/clients/base` so both
renderers (terminal cell-grid, web) receive already-gated affordance sets
per row — no renderer-side condition logic, consistent with the shared-core
boundary (state/reducer/view_model in base, clients transport+render).

## Trigger

Either a third conditional affordance appears on a list view, or a no-op
activation confuses an operator in practice. One dormant affordance on one
view does not pay for a vocabulary extension.
