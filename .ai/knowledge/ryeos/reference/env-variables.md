---
category: "ryeos/reference"
name: "env-variables"
description: "Environment variables for daemon, CLI, and runtimes"
---

# Environment Variables

## Required

| Variable | Description |
|---|---|
| `HOSTNAME` | Node identity for thread isolation. Must be non-empty. |

## Daemon configuration

| Variable | Default | Description |
|---|---|---|
| `RYEOS_SYSTEM_SPACE_DIR` | `$XDG_DATA_DIR/ryeos` | System data directory. Overrides `--system-data-dir` CLI flag. |
| `RYEOS_PUBLISHER_KEY_PATH` | Node identity key | Override operator signing key path. |
| `XDG_RUNTIME_DIR` | `/tmp/ryeosd-<uid>` | UDS socket parent directory. |
| `RUST_LOG` | — | Tracing filter. Propagated to subprocesses. |
| `RUST_BACKTRACE` | — | Backtrace control. Propagated to subprocesses. |

## CLI configuration

| Variable | Default | Description |
|---|---|---|
| `RYEOS_STATE_DIR` | `$XDG_STATE_DIR/ryeosd` | Daemon state directory for CLI verbs. |
| `RYEOS_CLI_KEY_PATH` | Node identity key | CLI signing key path. |
| `RYEOSD_SOCKET_PATH` | `$XDG_RUNTIME_DIR/ryeosd.sock` | Daemon UDS socket path. |
| `RYEOS_PUBLISHER_KEY` | `~/.ai/config/keys/signing/private_key.pem` | Publisher signing key for `ryeos sign`. |
| `HOME` | — | Fallback for user root discovery. |

## Runtime subprocess (injected by daemon)

These are set automatically by the protocol builder. Do not set manually.

| Variable | Description |
|---|---|
| `RYEOSD_CALLBACK_TOKEN` | Auth token for daemon callbacks. |
| `RYEOSD_THREAD_AUTH_TOKEN` | Per-thread auth token. |
| `RYEOSD_SOCKET_PATH` | Daemon UDS path for callbacks. |
| `RYEOSD_THREAD_ID` | Thread identifier. |
| `RYEOSD_PROJECT_PATH` | Project root path. |
| `RYEOS_THREAD_ID` | Thread ID for tool primitives. |
| `RYEOS_CHAIN_ROOT_ID` | Chain root identifier. |
| `RYEOS_ITEM_PATH` | Resolved item source path. |
| `RYEOS_ITEM_KIND` | Resolved item kind. |
| `RYEOS_ITEM_REF` | Canonical item reference. |
| `RYEOS_PROJECT_ROOT` | Materialized project root. |
| `RYEOS_SITE_ID` | Current site identifier. |
| `RYEOS_ORIGIN_SITE_ID` | Origin site identifier. |
| `USER_SPACE` | User-space root path. |
| `RYEOS_SYSTEM_SPACE_DIR` | System-space root path. |
| `RYEOS_CHECKPOINT_DIR` | Per-thread checkpoint directory. |
| `RYEOS_RESUME` | Set to `1` on resume re-spawns. |

## Provider auth (dynamic)

LLM provider auth uses dynamic env var names from the provider's `auth.env_var` field:

| Common Variable | Provider |
|---|---|
| `OPENAI_API_KEY` | OpenAI |
| `ANTHROPIC_API_KEY` | Anthropic |
| `OPENROUTER_API_KEY` | OpenRouter |

The runtime reads `std::env::var(<env_var>)` and fails if unset.

## Subprocess env allowlist

The daemon propagates these OS-level vars to every subprocess:

`PATH`, `HOME`, `LANG`, `LC_ALL`, `LC_CTYPE`, `TZ`, `TMPDIR`, `USER_SPACE`, `RYEOS_SYSTEM_SPACE_DIR`, `RUST_LOG`, `RUST_BACKTRACE`, `RYEOSD_TEST_STDERR_DIR`
