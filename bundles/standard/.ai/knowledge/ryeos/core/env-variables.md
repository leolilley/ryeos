<!-- ryeos:signed:2026-07-16T02:18:48Z:6629dbbd3bd289d9e32d1c349c4364287aed9344a5755d77db2c84b5fe8e5bbe:9uuBPsjxlolYUF8QKbhX5Zz3uW7LNJZMJk5bFnqPi7aP57KZXBOjOHKmKwgksOBRqnmF+Dd6KCuctvIXHoCNCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [reference, env, daemon, cli, runtimes, lifecycle]
version: "2.1.0"
description: >
  Environment variables for local lifecycle, daemon dispatch, CLI
  signing, runtimes, tools, and provider auth.
---

# Environment Variables

## Required by runtime execution

| Variable | Description |
|---|---|
| `HOSTNAME` | Node identity for thread isolation. Must be non-empty. |

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
`RYEOSD_CALLBACK_TOKEN`, `RYEOSD_THREAD_AUTH_TOKEN`, `RYEOSD_THREAD_ID`, and
`RYEOSD_PROJECT_PATH`) are declared by callback-capable protocols such as
`runtime` and the default tool protocol `tool_callback`; callback-free
protocols receive none of that authority. `RYEOSD_PROJECT_PATH` is the callback
authorization/state anchor: a deliberate state-root override when present,
otherwise the effective project root. It may intentionally differ from the
source-oriented `RYE_PROJECT_PATH`.

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

This is the construction allowlist. When node sandbox policy is enforced,
`environment.allow` is a second node-owned filter over the completed target
environment. Bubblewrap itself starts env-empty; accepted variables are set for
the target inside the namespace. Enforced mode replaces any inherited
`TMPDIR` value with `/tmp`, the sandbox-private tmpfs. When a verified protocol
requests callback IPC, its `RYEOSD_SOCKET_PATH` value
is checked against the daemon-pinned path before the exact socket is exposed.
Callback-free launches do not mount it. See [Execution
Sandbox](node/execution-isolation.md).
