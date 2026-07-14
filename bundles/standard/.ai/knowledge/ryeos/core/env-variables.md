<!-- ryeos:signed:2026-07-14T01:54:46Z:246061089c19421c9562493c078126b1d7cf446ab9e79f5a48e1a50e26c4069b:kIMAZjGYVw86d9YQbZC1W+k41yB5HGl5lAN70nUcEuvQRcjOjzSQjUYoiPxKmZyxPmHtlK+hPui+9rXfxhwSCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

The daemon sets `RYEOSD_URL` and `RYEOSD_SOCKET_PATH` after listener
startup and injects callback/runtime variables into subprocesses:
`RYEOSD_CALLBACK_TOKEN`, `RYEOSD_THREAD_AUTH_TOKEN`, `RYEOSD_THREAD_ID`,
`RYEOSD_PROJECT_PATH`, `RYEOS_THREAD_ID`, `RYEOS_CHAIN_ROOT_ID`,
`RYEOS_ITEM_PATH`, `RYEOS_ITEM_KIND`, `RYEOS_ITEM_REF`,
`RYEOS_PROJECT_ROOT`, `RYEOS_SITE_ID`, `RYEOS_ORIGIN_SITE_ID`,
`RYEOS_APP_ROOT`, `RYEOS_CHECKPOINT_DIR`, and
`RYEOS_RESUME`.

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
`TMPDIR` value with `/tmp`, the sandbox-private tmpfs. The
`RYEOSD_SOCKET_PATH` value is also checked against the daemon-pinned path before
the exact socket is exposed. See [Execution
Sandbox](node/execution-sandbox.md).
