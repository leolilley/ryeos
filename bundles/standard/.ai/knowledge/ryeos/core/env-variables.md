<!-- ryeos:signed:2026-08-29T09:08:38Z:caf9eecbe5d3b5a51fec59e50700fdabbaa683f9a5547b23c0cee3941f81b34b:VWQoFYWO0g8aQWB+a5OOk6NCVbgyfFfm/K6aXm4501yqJdB53SNE8e0LKsJE7CfZyPzZruEKyOlS8NvR0sCDAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [reference, env, daemon, cli, runtimes, lifecycle]
version: "2.2.0"
description: >
  Environment variables for local lifecycle, daemon dispatch, CLI
  signing, runtimes, tools, and provider auth.
---

# Environment Variables

## Required by runtime execution

| Variable | Description |
|---|---|
| `HOSTNAME` | Canonical node site name used to form `site:<HOSTNAME>` for thread and remote-origin isolation. It must be non-empty and stable across daemon restarts. |

Origin-bound remote grants must name the exact `site_id` reported by the
running source node. A supervisor must therefore start that node with the same
canonical host identity used when the grant was issued; changing `HOSTNAME`
does not rename a node or migrate its retained chains and instead causes exact
site-bound operations to fail closed.

## Local lifecycle and daemon configuration

| Variable | Default | Description |
|---|---|---|
| `RYEOS_APP_ROOT` | `$XDG_DATA_DIR/ryeos` | App root — operator state, installed bundles, keys, and trust. Equivalent to `--app-root`. |
| `XDG_RUNTIME_DIR` | `/tmp/ryeosd-<uid>` | Parent for default daemon UDS socket. |

`ryeos init`, `start`, `stop`, and `node status` ignore `RYEOSD_URL`.

## CLI daemon-backed dispatch

| Variable | Default | Description |
|---|---|---|
| `RYEOSD_URL` | discovered from `<app_root>/daemon.json` | Explicit daemon HTTP URL for normal dispatch; bypasses local lifecycle preflight. |

When `RYEOSD_URL` is unset, normal daemon-backed dispatch first requires
local lifecycle status `Running`, then reads `daemon.json` for bind.

## Daemon/runtime variables

The daemon sets `RYEOSD_URL` and `RYEOSD_SOCKET_PATH` for its own listener
process. A child receives only the environment selected by its verified
protocol plus daemon-root, engine-plan, secret, and resume bindings. In
particular, callback variables (`RYEOSD_SOCKET_PATH`,
`RYEOSD_CALLBACK_TOKEN`, `RYEOSD_THREAD_AUTH_TOKEN`, `RYEOSD_THREAD_ID`) and
the identity-only `RYEOSD_PROJECT_STATE_SCOPE` are declared by callback-capable protocols such as
`runtime` and the default tool protocol `tool_callback`; callback-free
protocols receive none of that authority. The daemon derives durable project
and state authority from the sealed callback token; subprocesses neither
receive nor submit the canonical host project path as callback authority.

`RYEOSD_PROJECT_STATE_SCOPE` is an opaque lowercase digest naming the admitted
logical project's durable state namespace. It stays stable when a pinned COW
continuation relocates or advances its operational snapshot. It is empty for
projectless execution and grants no state access by itself; the callback token
still supplies authority.

Engine-plan and lifecycle bindings can include `RYEOS_THREAD_ID`,
`RYEOS_CHAIN_ROOT_ID`, `RYEOS_ITEM_PATH`, `RYEOS_ITEM_KIND`, `RYEOS_ITEM_REF`,
`RYEOS_PROJECT_ROOT`, `RYEOS_SITE_ID`, `RYEOS_ORIGIN_SITE_ID`,
`RYEOS_APP_ROOT`, `RYEOS_CHECKPOINT_DIR`, and `RYEOS_RESUME` when applicable.

## Provider auth

LLM provider auth uses dynamic env var names from provider config, such
as `OPENAI_API_KEY` and `ANTHROPIC_API_KEY`.

## Subprocess env allowlist

The daemon propagates: `PATH`, `HOME`, `LANG`, `LC_ALL`, `LC_CTYPE`,
`TZ`, `TMPDIR`, `RUST_LOG`, `RUST_BACKTRACE`, `RYEOSD_TEST_STDERR_DIR`,
and the proxy/CA vars (`HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`,
`SSL_CERT_FILE`, `SSL_CERT_DIR`, and their lowercase forms).

This is the construction allowlist. When node isolation policy is enforced,
`environment.allow` is a second node-owned filter over the completed target
environment. The selected adapter receives no target environment; accepted
variables are represented in the typed launch plan. Enforced mode replaces any inherited
`TMPDIR` value with `/tmp`, the isolation-private tmpfs. When a verified protocol
requests callback IPC, its `RYEOSD_SOCKET_PATH` value
is checked against the daemon-pinned path before the exact socket is exposed.
Callback-free launches do not mount it. See [Execution
Isolation](node/execution-isolation.md).
