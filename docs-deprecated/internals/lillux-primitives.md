```yaml
id: lillux-primitives
title: "Lillux Primitives & Rye Runtime Modules"
description: The microkernel layer — execute, signing, and integrity — plus runtime modules that moved from Lillux to Rye
category: internals
tags: [lillux, primitives, microkernel, execute, rye, runtime]
version: "1.1.0"
```

# Lillux Primitives & Rye Runtime Modules

Lillux is the microkernel layer of Rye OS. It provides stateless, async-first primitives for interacting with the operating system. Lillux has **no knowledge** of Rye, `.ai/` directories, tool metadata, or space resolution — it receives fully-resolved configuration and executes it.

All Lillux code lives in `lillux/kernel/`.

> **Note:** As part of the kernel/userspace separation, several modules that were previously Lillux primitives have moved to Rye. Specifically, `env_resolver`, `auth`, and `schema_validator` now live under `rye/runtime/` or `rye/schemas/`. These were always userspace concerns — environment resolution, and authentication policy — and are now properly located in the Rye layer.

## ExecutePrimitive

**Location:** `lillux/primitives/execute.py`

Unified process management primitive. All process operations — inline execution, detached spawning, killing, and status checks — go through the `lillux` Rust binary. No POSIX fallbacks.

### Hard Dependency: lillux

`ExecutePrimitive.__init__()` resolves `lillux` via `shutil.which()`. If the binary is not found on `$PATH`, it raises `ConfigurationError`. This is intentional — lillux is a hard requirement for all process operations.

```python
class ExecutePrimitive:
    def __init__(self):
        self._lillux = shutil.which("lillux")
        if not self._lillux:
            raise ConfigurationError("lillux binary not found on PATH.")
```

### lillux CLI Flags

| Flag            | Subcommand | Description                                                                 |
| --------------- | ---------- | --------------------------------------------------------------------------- |
| `--stdin-pipe`  | `exec`     | Read stdin data from the process's real stdin instead of a `--stdin` argument. Avoids the 128KB per-argument OS limit. |

Environment variables are no longer passed as `--env` CLI arguments. Instead, lillux inherits the environment from its parent process (set via `asyncio.create_subprocess_exec(env=...)`).

### Interface

```python
class ExecutePrimitive:
    async def execute(self, config: Dict, params: Dict) -> ExecuteResult
    async def spawn(self, cmd, args, log_path=None, envs=None) -> SpawnResult
    async def kill(self, pid, grace=3.0) -> KillResult
    async def status(self, pid) -> StatusResult
```

### execute() — Inline Run-and-Wait

Delegates to `lillux exec`. Two-stage templating (env vars then runtime params) stays in Python; the actual process execution is handled by lillux.

When `input_data` is provided, it is piped through real stdin (via the `--stdin-pipe` flag on lillux) rather than passed as a `--stdin` CLI argument. This avoids the 128KB per-argument OS limit that would cause failures with large payloads.

Environment variables are passed via the `env` parameter to `asyncio.create_subprocess_exec()` so that lillux inherits them from its parent process, rather than serialized as `--env` CLI arguments. This avoids `E2BIG` errors when the combined environment + arguments would exceed OS limits.

**Config keys:**

| Key          | Type      | Default  | Description                                      |
| ------------ | --------- | -------- | ------------------------------------------------ |
| `command`    | str       | required | Command to execute                               |
| `args`       | list[str] | `[]`     | Command arguments                                |
| `cwd`        | str       | None     | Working directory                                |
| `input_data` | str       | None     | Data piped to stdin (via `--stdin-pipe`)          |
| `env`        | dict      | `{}`     | Environment variables (inherited via process env) |
| `timeout`    | int       | `300`    | Timeout in seconds                               |

### spawn() — Detached Process

Delegates to `lillux spawn`. Returns immediately with a PID.

```python
result = await primitive.spawn("python", ["worker.py"], log_path="/tmp/worker.log")
# SpawnResult(success=True, pid=12345)
```

### kill() — Graceful Then Force

Delegates to `lillux kill`. Sends SIGTERM, waits `grace` seconds, then SIGKILL.

```python
result = await primitive.kill(12345, grace=3.0)
# KillResult(success=True, pid=12345, method="terminated")
```

### status() — Is Process Alive

Delegates to `lillux status`.

```python
result = await primitive.status(12345)
# StatusResult(pid=12345, alive=True)
```

### Two-Stage Templating

1. **Stage 1 — Environment expansion:** `${VAR:-default}` patterns are replaced with environment values
2. **Stage 2 — Parameter substitution:** `{param_name}` patterns are replaced with runtime parameter values

Both stages run on `command`, `args`, `cwd`, and `input_data` within `execute()`.

### Environment Merge Heuristic

The merged environment dict is passed to `asyncio.create_subprocess_exec(env=...)` so that lillux inherits it directly. This replaces the previous approach of passing individual `--env KEY=VALUE` CLI arguments, which could trigger `E2BIG` errors when the total argument size exceeded OS limits.

```python
if len(config_env) < 50:
    # Merge config env over os.environ (partially resolved)
    result = os.environ.copy()
    result.update(config_env)
else:
    # Use config env directly (assumed fully resolved by orchestrator)
    result = config_env
```

### Result Types

```python
@dataclass
class ExecuteResult:
    success: bool       # True if return code is 0
    stdout: str         # Standard output
    stderr: str         # Standard error
    return_code: int    # Exit code
    duration_ms: float  # Execution time

@dataclass
class SpawnResult:
    success: bool
    pid: int | None = None
    error: str | None = None

@dataclass
class KillResult:
    success: bool
    pid: int = 0
    method: str = ""       # "terminated" | "killed" | "already_dead"
    error: str | None = None

@dataclass
class StatusResult:
    pid: int
    alive: bool
```

### Timeout Handling

lillux handles timeouts natively. On timeout, the child process is killed and an `ExecuteResult` is returned with `success=False` and stderr `"Command timed out after {timeout} seconds"`.

### Error Cases

| Condition            | Return Code | Stderr                                        |
| -------------------- | ----------- | --------------------------------------------- |
| lillux not on PATH      | N/A         | `ConfigurationError` raised at `__init__`     |
| Command not found    | -1          | `"Failed to spawn: {error}"`                 |
| Timeout              | -1          | `"Command timed out after {timeout} seconds"` |
| No command specified | -1          | `"No command specified"`                      |

## Signing Primitives

**Location:** `lillux/primitives/signing.py`

Pure Ed25519 cryptographic operations. No policy, no I/O beyond key material.

### Functions

| Function                                                  | Purpose                                                     |
| --------------------------------------------------------- | ----------------------------------------------------------- |
| `generate_keypair()`                                      | Generate new Ed25519 key pair → `(private_pem, public_pem)` |
| `sign_hash(content_hash, private_key_pem)`                | Sign a SHA256 hex digest → base64url-encoded signature      |
| `verify_signature(content_hash, sig_b64, public_key_pem)` | Verify signature → `True`/`False`                           |
| `compute_key_fingerprint(public_key_pem)`                 | SHA256 of public key PEM → first 16 hex chars               |
| `save_keypair(private_pem, public_pem, key_dir)`          | Save to disk with restricted permissions                    |
| `load_keypair(key_dir)`                                   | Load from `private_key.pem` and `public_key.pem`            |
| `ensure_keypair(key_dir)`                                 | *Deprecated* — use `load_keypair()` and handle missing keys explicitly |

### Key Storage

```
{key_dir}/
  private_key.pem  (mode 0600 — owner read/write only)
  public_key.pem   (mode 0644 — world readable)
```

The key directory itself is set to mode `0700`.

### Implementation

Uses the `cryptography` library's `Ed25519PrivateKey` and `Ed25519PublicKey`. Private keys are stored in PKCS8 PEM format without encryption. Signatures are base64url-encoded (URL-safe base64 without padding issues).

## Integrity Hashing

**Location:** `lillux/primitives/integrity.py`

Deterministic SHA256 hashing with canonical JSON serialization for arbitrary data. Lillux is type-agnostic — callers (e.g., rye) structure the data dict for their item types.

### Functions

| Function                  | Purpose                                                           |
| ------------------------- | ----------------------------------------------------------------- |
| `canonical_json(data)`    | Serialize any data to canonical JSON (sorted keys, no whitespace) |
| `compute_integrity(data)` | SHA256 hex digest of canonical JSON of any dict                   |

### Canonical JSON

All hashing uses canonical JSON serialization to ensure deterministic output:

```python
json.dumps(data, sort_keys=True, separators=(",", ":"), ensure_ascii=True)
```

This guarantees the same input always produces the same hash, regardless of dict ordering or whitespace in the original data.

## EnvResolver

**Location:** `rye/runtime/env_resolver.py`

Resolves environment variables from multiple sources. Pure resolver with no side effects — it doesn't create venvs or install packages.

### Resolution Order

1. **System environment** — Start with `os.environ.copy()`
2. **`.env` files** — Load from project root (simple `KEY=value` parsing, skip comments and `export` lines)
3. **ENV_CONFIG rules** — Apply interpreter resolution and static variables
4. **Tool-level overrides** — Highest priority, direct key-value merge

### Interpreter Resolution

The resolver supports four interpreter discovery strategies:

| Type              | Config Key           | Search Method                                                             |
| ----------------- | -------------------- | ------------------------------------------------------------------------- |
| `venv_python`     | `venv_path`          | Check `.venv/bin/python`, `.venv/Scripts/python.exe`, `.venv/bin/python3` |
| `node_modules`    | `search_paths`       | Check `node_modules/.bin/node`                                            |
| `system_binary`   | `binary`             | Run `which`/`where` to find on PATH                                       |
| `version_manager` | `manager`, `version` | Query pyenv, nvm, rbenv, or asdf                                          |

Each strategy resolves to a path and sets the environment variable named by `var`. If resolution fails, the `fallback` value is used.

### Static Variable Expansion

Static variables in `env_config.env` support `${VAR:-default}` expansion:

```yaml
env_config:
  env:
    PYTHONUNBUFFERED: "1"
    PROJECT_VENV_PYTHON: "${RYE_PYTHON}"
    MY_VAR: "${OTHER_VAR:-fallback_value}"
```

Variables are applied in order, so later variables can reference earlier ones.

## Auth

**Location:** `rye/runtime/auth.py`

Authentication primitives for the runtime layer.

## SchemaValidator

**Location:** `rye/schemas/schema_validator.py`

JSON Schema validation for tool configs, runtime parameters, and other structured data.

## Design Principle

Lillux is intentionally minimal. It provides OS-level capabilities that any orchestrator could use. The separation means:

- Lillux can be tested independently of Rye
- Lillux primitives can be reused in other projects
- Policy decisions (what to sign, when to verify, which spaces to check) live in Rye, not Lillux
- Lillux never imports from `rye.*` — the dependency is strictly one-way
