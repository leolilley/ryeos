# Environment Variables Reference

Complete catalogue of every environment variable read by `ryeosd`, `rye` (CLI),
`ryeos-directive-runtime`, `ryeos-graph-runtime`, `ryeos-engine`, and
`ryeos-tools`.

Compile-time constants set by `build.rs` (e.g. `CARGO_PKG_VERSION`,
`CARGO_MANIFEST_DIR`) are **not** included — they are baked into the binary and
cannot be overridden at runtime.

---

## 1. Required

Variables that **must** be set or the component aborts on startup.

| Variable | Default | Required | Description |
|---|---|---|---|
| `HOSTNAME` | — | **Yes** (daemon) | Node identity used to construct the `site_id` for thread isolation. Must be non-empty. Read once during `ThreadLifecycleService::new()`. |

---

## 2. Daemon configuration (`ryeosd`)

Variables that override paths, ports, or behaviour. All have sensible defaults
and are optional for a standard installation.

| Variable | Default | Required | Description |
|---|---|---|---|
| `XDG_RUNTIME_DIR` | `/tmp/ryeosd-<uid>` | No | Used to locate the UDS socket path (`$XDG_RUNTIME_DIR/ryeosd.sock`). Fallback to a temp directory keyed by UID when unset. |
| `RYE_SIGNING_KEY_PATH` | `<state_dir>/.ai/node/identity/private_key.pem` | No | Override the operator (user) signing key path. Used as a fallback when `config.yaml` omits `user_signing_key_path`. |
| `RYE_SYSTEM_SPACE` | `$XDG_DATA_DIR/ryeos` | No | Override the system data directory (where system bundles live). Takes precedence over `--system-data-dir` CLI flag and `config.yaml`. |
| `RUST_LOG` | — | No | `tracing` / `env_logger` filter directive. Propagated to spawned subprocesses via the allowlist. |
| `RUST_BACKTRACE` | — | No | Rust backtrace control. Propagated to spawned subprocesses. |

### Compile-time (set by `build.rs`, not overridable)

| Variable | Description |
|---|---|
| `RYEOSD_HOST_TRIPLE` | The host target triple (e.g. `x86_64-unknown-linux-gnu`) baked in at compile time. Used for native executor resolution from the system bundle CAS. |

---

## 3. CLI configuration (`rye`)

Variables read by the CLI binary to locate keys, sockets, and state.

| Variable | Default | Required | Description |
|---|---|---|---|
| `RYEOS_STATE_DIR` | `$XDG_STATE_DIR/ryeosd` | No | Daemon state directory. Used by `rye` verbs (`sign`, `inspect`, `vault`, local verbs, dispatcher) to discover bundles, identity, and config. Falls back to the XDG state directory. |
| `RYEOS_CLI_KEY_PATH` | `<state_dir>/.ai/node/identity/private_key.pem` | No | Path to the Ed25519 private key the CLI uses to sign requests. Falls back to the daemon's node signing key. |
| `RYEOSD_SOCKET_PATH` | `$XDG_RUNTIME_DIR/ryeosd.sock` (else `/tmp/ryeosd-<uid>/ryeosd.sock`) | No | Path to the daemon's Unix domain socket. Used by the CLI and runtime libraries to reach the daemon's JSON-RPC endpoint. |
| `RYE_SIGNING_KEY` | `$HOME/.ai/config/keys/signing/private_key.pem` | No | Path to the operator signing key used by `rye sign` and test fixtures. Falls back to `$HOME/.ai/config/keys/signing/private_key.pem`. |
| `HOME` | — | Conditional | Required by `rye sign` to locate the default signing key when `RYE_SIGNING_KEY` is unset. Also used as a fallback for user root discovery. |

---

## 4. Runtime subprocess configuration

Variables **read by spawned runtimes** (`ryeos-directive-runtime`,
`ryeos-graph-runtime`, tool primitives) to reach the daemon and identify their
execution context. These are injected by the daemon's protocol builder at spawn
time; operators do not set them directly.

| Variable | Default | Required | Description |
|---|---|---|---|
| `RYEOSD_CALLBACK_TOKEN` | — | **Yes** (runtimes) | Opaque auth token the runtime includes in every callback RPC to the daemon. Injected by the daemon from the minted callback capability. |
| `RYEOSD_THREAD_AUTH_TOKEN` | — | **Yes** (runtimes) | Per-thread auth token proving the subprocess's identity on callbacks. Required by `ryeos-directive-runtime`, `ryeos-graph-runtime`, and the `ryeos-runtime` callback client. |
| `RYEOSD_SOCKET_PATH` | — | **Yes** (runtimes) | Path to the daemon UDS socket. Injected by the protocol builder. Runtimes use this (with `RYEOSD_CALLBACK_TOKEN`) to build a `CallbackClient`. |
| `RYEOSD_THREAD_ID` | — | Injected | Thread identifier injected by the protocol builder's `env_injections`. |
| `RYEOSD_PROJECT_PATH` | — | Injected | Project root path injected by the protocol builder. |
| `RYE_THREAD_ID` | `graph-default` | Injected | Thread ID injected by the engine's `dispatch_subprocess` for tool primitives. Also used as clap `env` default for `--thread-id` in `ryeos-graph-runtime`. |
| `RYE_CHAIN_ROOT_ID` | — | Injected | Chain root identifier injected by the engine's `dispatch_subprocess` for tool primitives. |
| `RYE_ITEM_PATH` | — | Injected | Source path of the resolved item. Injected by the plan builder for tool primitives. |
| `RYE_ITEM_KIND` | — | Injected | Kind of the resolved item (e.g. `tool`, `directive`). |
| `RYE_ITEM_REF` | — | Injected | Canonical reference of the resolved item. |
| `RYE_PROJECT_ROOT` | — | Injected | Materialized project root for the execution. |
| `RYE_SITE_ID` | — | Injected | Current site identifier for the execution context. |
| `RYE_ORIGIN_SITE_ID` | — | Injected | Origin site identifier for the execution context. |
| `USER_SPACE` | — | Injected | User-space root path. Set by the daemon in the subprocess allowlist to ensure consistent root discovery. Also read by `ryeos-engine/roots` when resolving user root. |
| `RYE_SYSTEM_SPACE` | — | Injected | System-space root path. Set by the daemon in the subprocess allowlist and read by `ryeos-engine/roots` for system root discovery. |
| `RYE_CHECKPOINT_DIR` | — | Injected | Per-thread checkpoint directory allocated by the daemon for `native_resume` tools. Read by `CheckpointWriter::from_env()`. |
| `RYE_RESUME` | — | Injected | Set to `1` when the daemon re-spawns a tool as a resume. Checked via `CheckpointWriter::is_resume()`. |
| `RYE_CACHE_DIR` | `$TMPDIR/rye-graph-cache` | No | Graph runtime node cache directory. Defaults to a temp directory when unset. |
| `RYE_STATE` | — | No (tools lib) | Daemon state root used by `ryeos-tools::get_state_root()`. Required for tools that operate on the daemon's CAS state directory (e.g. GC, rebuild). |

### Subprocess allowlist

The daemon propagates the following OS-level env vars to every spawned
subprocess (in addition to the injected variables above):

`PATH`, `HOME`, `LANG`, `LC_ALL`, `LC_CTYPE`, `TZ`, `TMPDIR`, `USER_SPACE`,
`RYE_SYSTEM_SPACE`, `RUST_LOG`, `RUST_BACKTRACE`, `RYEOSD_TEST_STDERR_DIR`.

---

## 5. Provider / auth

Variables consumed by the directive runtime's provider adapter to authenticate
with LLM providers. The env var name is **dynamic** — it comes from the
provider's `auth.env_var` field in the directive configuration.

| Variable | Default | Required | Description |
|---|---|---|---|
| *(dynamic)* | — | Conditional | Any env var referenced in a provider's `auth.env_var` field (e.g. `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`). The runtime reads `std::env::var(<env_var>)` and fails loudly if unset. |

### HMAC verifier secrets

HMAC route verifiers read a secret from an env var whose name is configured in
the verifier's `secret_env` field. The variable name is deployment-specific.

| Variable | Default | Required | Description |
|---|---|---|---|
| *(dynamic)* | — | Conditional | Env var named by the HMAC verifier config's `secret_env` field. Must be non-empty at daemon start. |

---

## 6. Test / development

Variables used exclusively by test harnesses and development tooling. Not
required in production.

| Variable | Default | Required | Description |
|---|---|---|---|
| `RYEOSD_TEST_STDERR_DIR` | — | Test-only | When set, the daemon test harness mirrors daemon stderr to files under this directory for diagnostic capture. Propagated to spawned subprocesses via the allowlist. |
| `RYE_SIGNING_KEY` | `$HOME/.ai/config/keys/signing/private_key.pem` | Test-only | Used by `ryeos-tools` test fixtures (`test_support::signing_key_path`) to locate the platform-author signing key for isolated bundle re-signing. |
| `HOME` | — | Test-only | Used by `ryeos-tools` test support as a fallback to locate the signing key when `RYE_SIGNING_KEY` is unset. |

---

## 7. Internal

Variables that are **auto-set by the daemon** when spawning runtimes. Operators
should never set these manually; they are documented here for debugging and
auditing purposes.

| Variable | Set by | Description |
|---|---|---|
| `RYEOSD_CALLBACK_TOKEN` | Daemon (protocol builder) | Opaque callback auth token minted per-invocation with TTL and capability scoping. |
| `RYEOSD_THREAD_AUTH_TOKEN` | Daemon (protocol builder) | Per-thread auth token for subprocess identity verification on callbacks. |
| `RYEOSD_SOCKET_PATH` | Daemon (protocol builder) | Daemon's UDS path, injected so the runtime can reach the callback endpoint. |
| `RYEOSD_THREAD_ID` | Daemon (protocol builder) | Thread identifier for the launched execution. |
| `RYEOSD_PROJECT_PATH` | Daemon (protocol builder) | Project root for the launched execution. |
| `RYE_THREAD_ID` | Engine (`dispatch_subprocess`) | Thread ID injected into tool primitive subprocess env. |
| `RYE_CHAIN_ROOT_ID` | Engine (`dispatch_subprocess`) | Chain root ID for tool primitive subprocesses. |
| `RYE_ITEM_PATH` | Engine (`plan_builder`) | Resolved item source path. |
| `RYE_ITEM_KIND` | Engine (`plan_builder`) | Resolved item kind. |
| `RYE_ITEM_REF` | Engine (`plan_builder`) | Canonical item reference. |
| `RYE_PROJECT_ROOT` | Engine (`plan_builder`) | Materialized project root (set when applicable). |
| `RYE_SITE_ID` | Engine (`plan_builder`) | Current site ID for plan execution. |
| `RYE_ORIGIN_SITE_ID` | Engine (`plan_builder`) | Origin site ID for plan execution. |
| `USER_SPACE` | Daemon (`build_spawn_env`) | Resolved user-space root, set on every subprocess. |
| `RYE_SYSTEM_SPACE` | Daemon (`build_spawn_env`) | Resolved system-space root, set on every subprocess. |
| `RYE_CHECKPOINT_DIR` | Daemon (spawn logic) | Per-thread checkpoint directory for `native_resume` tools. |
| `RYE_RESUME` | Daemon (resume spawn) | Set to `"1"` on resume re-spawns. |

### Dynamic env vars injected by the protocol builder

The protocol vocabulary's `EnvInjectionSource` enum defines the full set of
values the builder can inject. The actual env var **names** are declared by each
protocol descriptor's `env_injections` array. The standard descriptor injects:

| Injection source | Typical env var name |
|---|---|
| `CallbackSocketPath` | `RYEOSD_SOCKET_PATH` |
| `CallbackToken` | `RYEOSD_CALLBACK_TOKEN` |
| `ThreadId` | `RYEOSD_THREAD_ID` |
| `ProjectPath` | `RYEOSD_PROJECT_PATH` |
| `ThreadAuthToken` | `RYEOSD_THREAD_AUTH_TOKEN` |

Other available sources (`CallbackTokenUrl`, `ActingPrincipal`, `CasRoot`,
`VaultHandle`, `StateDir`) are declared by specific protocol descriptors as
needed.

### Dynamic env vars injected by the plan builder

Tool primitive env is built by the engine's `plan_builder` (step 3), then
augmented by `dispatch_subprocess` which adds `RYE_THREAD_ID` and
`RYE_CHAIN_ROOT_ID`.

### `env_config` interpreter override

Runtime handler `env_config` supports an `interpreter.var` field. When set, the
engine reads `std::env::var(<var>)` to override the resolved interpreter binary.
This is typically a `RYE_PYTHON`-style variable whose name is declared in the
runtime's YAML config.

---

## See also

- [Installation guide](../getting-started/installation.md)
- [Daemon bootstrap](../operations/daemon-bootstrap.md)
