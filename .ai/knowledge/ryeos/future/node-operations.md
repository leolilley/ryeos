<!-- ryeos:signed:2026-07-21T00:24:56Z:0ebdb18b6c114e1c9b831c143158e069080dd30dbe1d188e76aaadc669deb44d:SKHPejvKmkC22fgDU4H3OxOoRlLH0dzoOaCDFCCsvIiqP59Z/AUl4E8adhaHXfl5d85RffyJWjD6BXKTt08VBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: node-operations
title: Future Node Operations Catalog
description: Deferred criteria and constraints for a non-CLI operation taxonomy
entry_type: design
version: "1.0.0"
```

# Future `node/operations` Catalog

## Status

Parking-lot design note. Do not implement unless a concrete non-CLI operation
catalog is needed.

The completed command migration intentionally made `.ai/node/commands` the CLI
command surface. Commands own user-facing syntax and dispatch intent.
Authorization remains based on the executable item ref and capability checks.

## When to add operations

Add `.ai/node/operations/*.yaml` only if at least one of these becomes real:

1. Capability checks need stable operation names that are not derivable from
   item refs.
2. Audit logs need a signed operation label separate from CLI command tokens and
   execution target.
3. Non-CLI clients need a discoverable operation catalog independent of command
   UX.
4. A single operation intentionally maps to multiple execution targets under
   node policy.
5. RyeOS UI needs operation grouping that cannot be represented by command groups
   or item metadata.

## Non-goals

- Do not reintroduce CLI aliases or command routing through operations.
- Do not make operations grant capabilities.
- Do not use operations as a compatibility wrapper for removed `node/verbs`.

## Relationship to commands

```text
commands   = CLI/user-facing syntax + dispatch intent
operations = optional future security/audit taxonomy
items      = actual executable refs and runtime authorization target
```
