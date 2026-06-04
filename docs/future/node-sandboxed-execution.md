# Node sandboxed execution

## Status

Deferred. The dispatch path already has a `sandbox_wrap(SubprocessSpec)` seam,
but it is currently an identity function.

## Goal

Introduce a node-level sandbox stage between RyeOS execution planning and the
`lillux` subprocess call.

The stage should be able to enforce, or deliberately decline to enforce:

- filesystem root and writable path restrictions;
- network policy;
- environment variable allowlists;
- process/resource limits;
- project/source provenance constraints;
- audit metadata attached to the spawned process.

## Constraints

- Keep `SubprocessSpec` as the single boundary between planning and spawn.
- Do not re-cut the tool/runtime dispatch path just to add sandboxing.
- Sandbox policy must be node-owned/operator-owned, not item-authored authority.
- Fail closed when a requested sandbox policy is unavailable.
