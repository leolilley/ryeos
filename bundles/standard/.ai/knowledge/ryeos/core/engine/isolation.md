<!-- ryeos:signed:2026-07-21T00:24:30Z:40786b51cb025dba7737ce3514144169592f9afe4b026579e69a2a1f325f8b4e:dkalhxICPSMF27cZu1xzUnt4ENsXkd275VCjFkM1jexRIsc2HSzkBzX7YyxB4ZniTmoOzXPn0rOfQMsfH/BrDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
category: ryeos/core/engine
tags: [architecture, isolation, hermetic, env, security, subprocess]
version: "2.3.0"
description: >
  Hermetic execution and optional OS isolation — env_clear, explicit env
  injection, node-owned policy, signed backend bundles, per-route semaphores,
  and callback
  capability boundaries.
---

# Execution Isolation

Rye OS enforces isolation at multiple layers to prevent privilege
escalation, secret leakage, and environment-dependent behavior.

## Node-owned OS isolation

Tool and runtime item launches can additionally pass through the immutable strict
node isolation snapshot. The default mode is disabled. In enforce mode, the
node—not the item—owns filesystem/network policy, environment filtering, and
the Lillux open-file cap. Policy is resolved once at startup and shared across
launch paths; edits require restart. Parser/composer handlers are trusted engine
infrastructure and retain the hermetic handler boundary below.

The engine emits a typed backend-neutral launch plan. The selected signed bundle
declares an adapter, launcher artifacts, target triples, and a capability upper
bound; live inspection may narrow but never broaden that authority. Backends
are independently authored and installed bundles. Items may narrow node policy
but may not select a backend, enable isolation, or request fallback.

At bootstrap and prospective bundle admission, the selected adapter and
payloads are signature-verified and copied into immutable sealed executable
handles. Registry construction receives that exact runtime snapshot. Managed
launch metadata records the policy, backend, signer, executable digests,
effective capability set, and a canonical plan digest whose argument and
environment plaintext has been redacted.

See [Execution Isolation](../node/execution-isolation.md) for the complete node-owner
schema, pickup behavior, diagnostics, and security limits.

## Attachment is not isolation

Daemon-owned process durability does not depend on isolation being enabled.
The executor requests an attachment-prepared launch, Lillux obtains the exact
target identity while that target is unable to execute, RyeOS persists the
identity, and only then is execution released. Disabled isolation uses the
native direct hold; enforced isolation uses the selected backend's target hold.
The lifecycle transition is otherwise identical.

This separation is deliberate: isolation controls what a process may access,
while attachment controls whether RyeOS durably owns the process before it can
run. No backend, bundle, helper binary, item kind, launch mode, or filesystem
layout is hardcoded into the lifecycle contract. See
[Attachment Before Execution](../execution/attachment-before-execution.md).

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
   receive only the typed bindings composed by the final environment-contract
   builder.

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
RUST_LOG, RUST_BACKTRACE, RYEOSD_TEST_STDERR_DIR,
HTTP_PROXY, HTTPS_PROXY, NO_PROXY, SSL_CERT_FILE, SSL_CERT_DIR
(and lowercase proxy forms)
```

### Protocol and daemon bindings

The verified protocol declares its own environment injections; the daemon
produces only those requested values. The table below is the union of common
protocol and lifecycle bindings, not a list granted to every subprocess:

| Variable | Purpose |
|---|---|
| `RYEOSD_SOCKET_PATH` | Daemon UDS path for callbacks |
| `RYEOSD_CALLBACK_TOKEN` | Auth token for daemon callbacks |
| `RYEOSD_THREAD_AUTH_TOKEN` | Per-thread auth token |
| `RYEOSD_THREAD_ID` | Thread identifier |
| `RYEOSD_PROJECT_PATH` | Callback authorization/state anchor; a deliberate state-root override when present, otherwise the effective project root |
| `RYE_THREAD_ID` | Thread ID for tool primitives |
| `RYEOS_ITEM_PATH` | Resolved item source path |
| `RYEOS_ITEM_KIND` | Resolved item kind |
| `RYEOS_ITEM_REF` | Canonical item reference |
| `RYEOS_PROJECT_ROOT` | Materialized project root |
| `RYEOS_SITE_ID` / `RYEOS_ORIGIN_SITE_ID` | Site identifiers |
| `RYEOS_APP_ROOT` | App root path |
| `RYEOS_CHECKPOINT_DIR` | Per-thread checkpoint directory |
| `RYEOS_RESUME` | Set to `1` on resume re-spawns |

For example, `runtime` and the default `tool_callback` declare their
`RYEOSD_*` callback bindings, while `opaque` and `tool_streaming` declare only
callback-free `RYE_*` identity/project bindings. Callback-free launches receive
no callback tokens or daemon-socket sandbox mount.

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

Child processes cannot escalate beyond their parent's capabilities. Callback
tokens carry `effective_caps` — the composed capability set from the kind's
permission model. Capability-gated callback operations enforce these caps at
dispatch:

1. Empty `effective_caps` = deny all capability-gated resource operations
2. Wildcard `"*"` in `effective_caps` = allow everything
3. Otherwise: structured + regex matching against the required caps

Exact-thread and chain-local lifecycle methods instead enforce their declared
callback-token, thread-auth, or two-proof access class. The daemon does not
trust the runtime to self-police; enforcement happens at the UDS callback trust
boundary. See [callback-auth](../protocols/callback-auth.md) for details.

## Resume Capability Preservation

When a daemon restarts and auto-resumes a thread, the resumed process
gets a fresh callback token but with the **same** `effective_caps` the
pre-crash run had. The caps are persisted in `ResumeContext` in the
runtime database and restored verbatim on resume — the reconciler does
not re-derive them.
