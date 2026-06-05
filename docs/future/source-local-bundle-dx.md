# Source-local bundle development follow-ups

## Status

Partially complete; only the remaining DX follow-ups are listed here.

Already landed:

- command descriptors and local command help no longer route through daemon
  alias parsing;
- project-aware command tails can auto-detect a cwd ancestor containing `.ai/`;
- `RYEOS_PROJECT_ROOT` / `RYEOS_PROJECT_PATH` are part of the runtime/env
  contract;
- bundle build/publish smoke coverage exists in core tool tests.

## Deferred work

### 1. Explicit runtime state-root override

Source-local execution currently treats the selected project path as the
materialized project root. Bundle authors sometimes need to run a bundle from a
source root while placing runtime state under a temporary smoke directory.

Add a deliberate runtime state-root control instead of relying on ad hoc env
vars such as `PROJECT_ROOT`.

Desired shape, exact flag TBD:

```bash
ryeos execute tool:example/system/health \
  --state-root /tmp/example-smoke \
  --input params.json
```

The selected source/project root and selected runtime state root must both be
visible in execution diagnostics.

### 2. Multi-item signing UX

Bundle-author iteration still benefits from a command that signs a bounded set
of changed bundle files without requiring one command per file.

The implementation must preserve trust boundaries:

- no signing through symlinks;
- no signing outside the selected `.ai` root;
- clear dry-run output;
- deterministic file ordering.

### 3. First-class bundle smoke command

Add a user-facing smoke command that combines:

- source-local project detection;
- optional temporary runtime state root;
- bundle verification/preflight;
- one or more declared smoke executions;
- cleanup/reporting.

This should be a RyeOS command/service, not a one-off direct Python harness.
