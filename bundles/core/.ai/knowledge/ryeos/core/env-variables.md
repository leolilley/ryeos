---
category: ryeos/core
tags: [reference, env, daemon, cli, runtimes, lifecycle]
version: "2.0.0"
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
| `RYEOS_SYSTEM_SPACE_DIR` | `$XDG_DATA_DIR/ryeos` | System space root. Equivalent to `--system-space-dir`. |
| `USER_SPACE` | `~/.ryeos` via root resolver | User-space root override. |
| `XDG_RUNTIME_DIR` | `/tmp/ryeosd-<uid>` | Parent for default daemon UDS socket. |
| `RYEOS_SIGNING_KEY_PATH` | derived from user root | Daemon config override for `user_signing_key_path`. |

`ryeos init`, `start`, `stop`, and `status` ignore `RYEOSD_URL`.

## CLI daemon-backed dispatch

| Variable | Default | Description |
|---|---|---|
| `RYEOSD_URL` | discovered from `<system>/daemon.json` | Explicit daemon HTTP URL for normal dispatch; bypasses local lifecycle preflight. |
| `RYEOS_CLI_KEY_PATH` | `<user>/.ai/config/keys/signing/private_key.pem` | Explicit CLI/user signing key path. |

When `RYEOSD_URL` is unset, normal daemon-backed dispatch first requires
local lifecycle status `Running`, then reads `daemon.json` for bind.

## Daemon/runtime variables

The daemon sets `RYEOSD_URL` and `RYEOSD_SOCKET_PATH` after listener
startup and injects callback/runtime variables into subprocesses:
`RYEOSD_CALLBACK_TOKEN`, `RYEOSD_THREAD_AUTH_TOKEN`, `RYEOSD_THREAD_ID`,
`RYEOSD_PROJECT_PATH`, `RYEOS_THREAD_ID`, `RYEOS_CHAIN_ROOT_ID`,
`RYEOS_ITEM_PATH`, `RYEOS_ITEM_KIND`, `RYEOS_ITEM_REF`,
`RYEOS_PROJECT_ROOT`, `RYEOS_SITE_ID`, `RYEOS_ORIGIN_SITE_ID`,
`USER_SPACE`, `RYEOS_SYSTEM_SPACE_DIR`, `RYEOS_CHECKPOINT_DIR`, and
`RYEOS_RESUME`.

## Provider auth

LLM provider auth uses dynamic env var names from provider config, such
as `OPENAI_API_KEY` and `ANTHROPIC_API_KEY`.

## Subprocess env allowlist

The daemon propagates: `PATH`, `HOME`, `LANG`, `LC_ALL`, `LC_CTYPE`,
`TZ`, `TMPDIR`, `USER_SPACE`, `RYEOS_SYSTEM_SPACE_DIR`, `RUST_LOG`,
`RUST_BACKTRACE`, and `RYEOSD_TEST_STDERR_DIR`.
