# Node sandboxed execution

> Implemented. The normative operator and security contract is now
> [Execution sandbox contract](../security/execution-sandbox.md).

## Status

Implemented for Linux through a node-owned Bubblewrap policy at
`<app-root>/.ai/node/sandbox.yaml`. Missing policy, unsupported policy version,
missing backend, disallowed environment bindings, and working directories
outside writable roots all fail closed before spawn. Configured open-file
limits are enforced with `RLIMIT_NOFILE` at the Lillux spawn boundary.

The version 1 `max_processes` field remains accepted for compatibility but is
not enforced per sandbox. `ryeos node doctor` warns when it is configured. A
correct implementation is deferred to delegated cgroup v2 `pids.max`; using
`RLIMIT_NPROC` would affect the daemon's real UID rather than one sandbox.

## Goal

Introduce a node-level sandbox stage between RyeOS execution planning and the
`lillux` subprocess call.

The stage should be able to enforce, or deliberately decline to enforce:

- a read-only host filesystem with explicit writable path bindings;
- isolated or explicitly shared networking;
- exact or prefix-based environment variable allowlists;
- open-file limits, with process-count limits deferred to delegated cgroup v2;
- project/source provenance constraints;
- audit metadata attached to the spawned process.

## Constraints

- Keep `SubprocessSpec` as the single boundary between planning and spawn.
- Do not re-cut the tool/runtime dispatch path just to add sandboxing.
- Sandbox policy must be node-owned/operator-owned, not item-authored authority.
- Fail closed when the node sandbox policy or backend is unavailable.

The initial policy written by `ryeos init` permits runtime networking and the
daemon-constructed environment while restricting filesystem visibility to the
runtime system surface, the verified executable, and the active project.
Operators can narrow network and environment access in the node policy. These
defaults are operator authority, not item-authored requirements.
