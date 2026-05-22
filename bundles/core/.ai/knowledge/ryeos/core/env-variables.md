<!-- ryeos:signed:2026-05-22T03:35:36Z:c5362c6b9031458ccf2dc3b5e7654dcf82c00dfb34543a252bd25da895957131:PMe5KnsdcS5lvbFs1gp2EEviOMutt+4IpzAWWTRtX0N+KqUll8KurKO3jByltdruIeGroJcH8FhzulFKk8tYDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
category: ryeos/core
tags: [reference, env, daemon, cli, runtimes]
version: "1.0.0"
description: >
  Environment variables for daemon, CLI, runtimes, tools, and provider auth.
---

# Environment Variables

## Required

| Variable   | Description                                            |
| ---------- | ------------------------------------------------------ |
| `HOSTNAME` | Node identity for thread isolation. Must be non-empty. |

## Daemon configuration

| Variable                   | Default               | Description                                                    |
| -------------------------- | --------------------- | -------------------------------------------------------------- |
| `RYEOS_SYSTEM_SPACE_DIR`   | `$XDG_DATA_DIR/ryeos` | System data directory. Overrides `--system-data-dir` CLI flag. |
| `XDG_RUNTIME_DIR`          | `/tmp/ryeosd-<uid>`   | UDS socket parent directory.                                   |
| `RUST_LOG`                 | —                     | Tracing filter. Propagated to subprocesses.                    |
| `RUST_BACKTRACE`           | —                     | Backtrace control. Propagated to subprocesses.                 |

## CLI configuration

| Variable              | Default                                            | Description                             |
| --------------------- | -------------------------------------------------- | --------------------------------------- |
| `RYEOS_CLI_KEY_PATH`  | Node identity key                                  | CLI signing key path.                   |
| `RYEOSD_SOCKET_PATH`  | `$XDG_RUNTIME_DIR/ryeosd.sock`                     | Daemon UDS socket path.                 |
| `HOME`                | —                                                  | Fallback for user root discovery.       |

## Runtime subprocess (injected by daemon)

These are set automatically by the protocol builder. Do not set manually.

| Variable                   | Description                      |
| -------------------------- | -------------------------------- |
| `RYEOSD_CALLBACK_TOKEN`    | Auth token for daemon callbacks. |
| `RYEOSD_THREAD_AUTH_TOKEN` | Per-thread auth token.           |
| `RYEOSD_SOCKET_PATH`       | Daemon UDS path for callbacks.   |
| `RYEOSD_THREAD_ID`         | Thread identifier.               |
| `RYEOSD_PROJECT_PATH`      | Project root path.               |
| `RYEOS_THREAD_ID`          | Thread ID for tool primitives.   |
| `RYEOS_CHAIN_ROOT_ID`      | Chain root identifier.           |
| `RYEOS_ITEM_PATH`          | Resolved item source path.       |
| `RYEOS_ITEM_KIND`          | Resolved item kind.              |
| `RYEOS_ITEM_REF`           | Canonical item reference.        |
| `RYEOS_PROJECT_ROOT`       | Materialized project root.       |
| `RYEOS_SITE_ID`            | Current site identifier.         |
| `RYEOS_ORIGIN_SITE_ID`     | Origin site identifier.          |
| `USER_SPACE`               | User-space root path.            |
| `RYEOS_SYSTEM_SPACE_DIR`   | System-space root path.          |
| `RYEOS_CHECKPOINT_DIR`     | Per-thread checkpoint directory. |
| `RYEOS_RESUME`             | Set to `1` on resume re-spawns.  |

## Provider auth (dynamic)

LLM provider auth uses dynamic env var names from the provider's `auth.env_var` field:

| Common Variable     | Provider  |
| ------------------- | --------- |
| `OPENAI_API_KEY`    | OpenAI    |
| `ANTHROPIC_API_KEY` | Anthropic |

The runtime reads `std::env::var(<env_var>)` and fails if unset.

## Subprocess env allowlist

The daemon propagates these OS-level vars to every subprocess:

`PATH`, `HOME`, `LANG`, `LC_ALL`, `LC_CTYPE`, `TZ`, `TMPDIR`, `USER_SPACE`, `RYEOS_SYSTEM_SPACE_DIR`, `RUST_LOG`, `RUST_BACKTRACE`, `RYEOSD_TEST_STDERR_DIR`
