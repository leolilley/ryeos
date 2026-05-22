<!-- ryeos:signed:2026-05-22T04:30:07Z:ab23a0973ebdb205c1ffe5328b14b24137f87c7897d87e1a818c52e449afe2e6:J7FI2Fd91GE4bVMvOndpS+ItCkQFoGZmdFk8o14GM9XLSrU1O11GV3NBy43Ld6X6V9ILDRV2V4i2+ADUAoyvAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
category: ryeos/core/engine
tags: [architecture, isolation, hermetic, env, security, subprocess]
version: "1.0.0"
description: >
  Hermetic execution properties — env_clear, explicit env injection,
  per-route semaphores, callback capability boundaries, and the
  subprocess env allowlist.
---

# Execution Isolation

Rye OS enforces isolation at multiple layers to prevent privilege
escalation, secret leakage, and environment-dependent behavior.

## Hermetic Handler Execution

Every handler binary (parsers, composers) runs with `Command::env_clear()`
— a completely scrubbed environment. Only explicitly declared environment
variables are passed through.

### How It Works

The `SubprocessRequest.envs` field is **authoritative**: the runner
calls `env_clear()` before applying the envs, so callers MUST populate
every variable the subprocess needs. An empty `envs` vec means the
subprocess receives **zero** environment variables from the parent.

This applies at two levels:

1. **Handler binaries** — parser and composer handlers receive an empty
   env vec. No daemon secrets, no API keys, no PATH.
2. **Runtime subprocesses** — directive, graph, and knowledge runtimes
   receive only the vars explicitly composed by `build_spawn_env()`.

### What This Prevents

- **Secret leakage**: shell-exported variables on the daemon process
  cannot bypass `required_secrets` scoping
- **Non-determinism**: parser/composer behavior is independent of the
  daemon's environment
- **Reproducibility**: the same handler binary produces the same output
  regardless of what machine it runs on

## Subprocess Env Allowlist

For runtime subprocesses (directive-runtime, graph-runtime,
knowledge-runtime), the daemon composes the environment from an explicit
allowlist plus dynamic injection:

### Daemon-propagated vars

Only these OS-level vars are passed to every subprocess:

```
PATH, HOME, LANG, LC_ALL, LC_CTYPE, TZ, TMPDIR,
USER_SPACE, RYEOS_SYSTEM_SPACE_DIR, RUST_LOG, RUST_BACKTRACE
```

### Daemon-injected vars

These are set automatically by the protocol builder:

| Variable | Purpose |
|---|---|
| `RYEOSD_SOCKET_PATH` | Daemon UDS path for callbacks |
| `RYEOSD_CALLBACK_TOKEN` | Auth token for daemon callbacks |
| `RYEOSD_THREAD_AUTH_TOKEN` | Per-thread auth token |
| `RYEOSD_THREAD_ID` | Thread identifier |
| `RYEOSD_PROJECT_PATH` | Project root path |
| `RYE_THREAD_ID` | Thread ID for tool primitives |
| `RYEOS_ITEM_PATH` | Resolved item source path |
| `RYEOS_ITEM_KIND` | Resolved item kind |
| `RYEOS_ITEM_REF` | Canonical item reference |
| `RYEOS_PROJECT_ROOT` | Materialized project root |
| `RYEOS_SITE_ID` / `RYEOS_ORIGIN_SITE_ID` | Site identifiers |
| `USER_SPACE` | User-space root path |
| `RYEOS_CHECKPOINT_DIR` | Per-thread checkpoint directory |
| `RYEOS_RESUME` | Set to `1` on resume re-spawns |

### Host env passthrough (tools only)

Tool `env_config.env` values can request host environment passthrough
via `${VAR}` syntax, but only for vars in the
`RYEOS_TOOL_ENV_PASSTHROUGH` allowlist. Reserved `RYEOS_*` names are
rejected.

## Per-Route Semaphores

See [routes.md](../node/routes.md) for details. Each route gets its own
`tokio::sync::Semaphore` from `limits.concurrent_max`. Non-blocking
acquisition returns 503 if saturated. Per-route isolation means a
heavy upload endpoint cannot starve a lightweight health check.

## Callback Capability Boundaries

Child processes cannot escalate beyond their parent's capabilities.
Callback tokens carry `effective_caps` — the composed capability set from
the kind's permission model. When the child process calls back to the
daemon, the dispatcher enforces these caps before dispatch:

1. Empty `effective_caps` = deny-all
2. Wildcard `"*"` in `effective_caps` = allow everything
3. Otherwise: structured + regex matching against the required caps

The daemon does not trust the runtime to self-police. Enforcement
happens at the trust boundary (the UDS callback). See
[callback-auth](../protocols/callback-auth.md) for details.

## Resume Capability Preservation

When a daemon restarts and auto-resumes a thread, the resumed process
gets a fresh callback token but with the **same** `effective_caps` the
pre-crash run had. The caps are persisted in `ResumeContext` in the
runtime database and restored verbatim on resume — the reconciler does
not re-derive them.

If a persisted row lacks `effective_caps` (pre-V5.5 data), it defaults
to an empty `Vec` which is deny-all. This is the safe default.
