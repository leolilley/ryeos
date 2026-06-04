# Future `node/operations` catalog

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
5. Studio needs operation grouping that cannot be represented by command groups
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
