# Daemon Bootstrap Sequence

## Overview

The `ryeosd` daemon bootstrap is a multi-phase process that takes the binary from launch to "ready to serve requests." It resolves configuration, initializes node identity and trust, loads the engine and node config, opens the state store, acquires an exclusive lock, then binds HTTP and Unix domain socket listeners. Each phase is fail-closed: any error prevents startup.

---

## Phase 0: Config Resolution

Config resolution happens in `Config::load()` (`ryeosd/src/config.rs`). The daemon assembles its configuration by merging values from multiple sources in priority order:

**Resolution order for each key:**

| Key | CLI flag | Env var | Config file key | Compiled default |
|---|---|---|---|---|
| `bind` | `--bind` | — | `bind` | `127.0.0.1:7400` |
| `state_dir` | `--state_dir` | — | `state_dir` | `$XDG_STATE_DIR/ryeosd` |
| `system_data_dir` | `--system_data_dir` | `RYE_SYSTEM_SPACE` | `system_data_dir` | `$XDG_DATA_DIR/ryeos` |
| `uds_path` | `--uds_path` | — | `uds_path` | `$XDG_RUNTIME_DIR/ryeosd.sock` |
| `db_path` | `--db_path` | — | `db_path` | `<state_dir>/.ai/state/runtime.sqlite3` |
| `node_signing_key_path` | — | — | `node_signing_key_path` | `<state_dir>/.ai/node/identity/private_key.pem` |
| `user_signing_key_path` | — | `RYE_SIGNING_KEY_PATH` | `user_signing_key_path` | `~/.ai/config/keys/signing/private_key.pem` |
| `authorized_keys_dir` | `--authorized_keys_dir` | — | `authorized_keys_dir` | `<state_dir>/.ai/node/auth/authorized_keys` |
| `require_auth` | `--require_auth` | — | `require_auth` | `false` |

For most keys, the precedence is: **CLI flag > config file value > compiled default.** Notable exceptions:

- `system_data_dir`: `RYE_SYSTEM_SPACE` env var takes priority over `--system_data_dir` CLI flag, which takes priority over the config file.
- `user_signing_key_path`: `RYE_SIGNING_KEY_PATH` env var takes priority over the config file (no CLI flag).
- `bind`: if both CLI and config file specify `--bind` and they disagree, the daemon refuses to start unless `--force` is also passed. When `--force` is used, the CLI value wins and the config file is rewritten.

The config file is loaded from the path given by `--config`, falling back to `<state_dir>/.ai/node/config.yaml` if it exists. This means `state_dir` must be resolved first (from CLI or default) before the config file path can be determined.

After resolution, the daemon creates the db parent directory and the UDS parent directory (with mode `0700`).

**What can go wrong:**

- `could not determine base directories` — `BaseDirs::new()` failed. Ensure `HOME` is set.
- `could not determine XDG state directory` — `XDG_STATE_DIR` is not set and the OS doesn't provide a default.
- `failed to parse config file <path>` — YAML syntax error in the config file.
- `conflict between CLI --bind (...) and stored config.yaml (...)` — both specify `bind` with different values and `--force` was not passed.

**Controls:** `--config`, `--state_dir`, `--bind`, `--system_data_dir`, `--db_path`, `--uds_path`, `--require_auth`, `--authorized_keys_dir`, `--force`, `RYE_SYSTEM_SPACE`, `RYE_SIGNING_KEY_PATH`

---

## Phase 1: Init (if needed)

If `--init-if-missing` is passed and either the node signing key or vault private key is absent, the daemon calls `bootstrap::init()` (`ryeosd/src/bootstrap.rs`). If `--init-only` is passed, the daemon runs init unconditionally and exits.

**What init does (in order):**

1. **Creates the directory layout:**
   - `<state_dir>/.ai/node/auth/authorized_keys/`
   - `<state_dir>/.ai/node/vault/`
   - `<state_dir>/.ai/state/objects/`
   - `<state_dir>/.ai/state/refs/`

2. **Writes default config file** to `<state_dir>/.ai/node/config.yaml` if it doesn't already exist (or if `--force`).

3. **Creates the auth directory** (`authorized_keys_dir`).

4. **Generates or loads the node signing key** (Ed25519) at `<state_dir>/.ai/node/identity/private_key.pem`. With `--force`, the existing key is deleted and regenerated — but only if no signed node-config items exist under `<state_dir>/.ai/node/` (the daemon refuses to rotate the key if it would invalidate existing signatures).

5. **Generates or loads the user signing key** (Ed25519) at `~/.ai/config/keys/signing/private_key.pem`. This key is **never** force-regenerated — it represents the operator's persistent identity.

6. **Writes the public identity document** to `<state_dir>/.ai/node/identity/public-identity.json` — a signed JSON document containing the node's fingerprint, verifying key, and creation timestamp.

6b. **Generates or loads the vault X25519 keypair** at `<state_dir>/.ai/node/vault/private_key.pem` and `public_key.pem`. This key is separate from the Ed25519 node identity so that node-key rotation does not brick the vault.

7. **Bootstraps self-trust:** writes self-signed trusted-key TOML entries to `~/.ai/config/keys/trusted/<fingerprint>.toml` for both the node key and the user key. These allow the daemon's own signed items to verify on subsequent boots.

**What can go wrong:**

- `refusing --force: existing signed node-config items would become unverifiable` — you have signed YAMLs under `.ai/node/` and tried `--force`. Use `rye daemon rotate-key` instead, which re-signs everything.
- `signing key already exists at <path>` — the key file exists but `--force` was not passed. (This should not happen in the `--init-if-missing` path since that path only calls init when the key is absent.)

**Controls:** `--init-only`, `--init-if-missing`, `--force`

**Log lines for healthy init:**
```
INFO ryeosd::bootstrap: wrote default config path=...
INFO ryeosd::bootstrap: node signing key ready fingerprint=fp:... path=...
INFO ryeosd::bootstrap: user signing key ready fingerprint=fp:... path=...
INFO ryeosd::bootstrap: wrote node public identity path=...
INFO ryeosd::bootstrap: vault X25519 keypair ready fingerprint=fp:... path=...
INFO ryeosd::bootstrap: wrote self-signed trust entry path=... fingerprint=...
INFO ryeosd::bootstrap: wrote self-signed trust entry path=... fingerprint=...
```

---

## Phase 2: Trust Store Loading

After verifying initialization (`bootstrap::verify_initialized`), the daemon loads the trust store via `TrustStore::load_three_tier()` (in `ryeos-engine/src/trust.rs`), called from `bootstrap::load_node_config_two_phase()`.

The three-tier trust model sources trusted keys from **operator tiers only** (project > user). System roots are never a trust source — they are only checked for legacy bundle-internal trust dirs, which produce a warning.

**Directories scanned (in order, first match wins per fingerprint):**

1. **Project root** (not available at daemon startup — `None` is passed). Project-level trust is resolved per-request, not at boot.
2. **User root** (`~/.ai/config/keys/trusted/`) — this is where the daemon's self-trust entries and operator-pinned publisher keys live.

Each `.toml` file in these directories declares a trusted verifying key. The trust store is a flat map from fingerprint to `TrustedSigner`.

**What can go wrong:**

- `failed to load bootstrap trust store for node-config verification` — the trust dir exists but contains malformed TOML, or I/O errors prevented reading.
- `ignoring bundle-internal trust dir` — a legacy trust directory was found inside a system root. This is a warning, not an error. Pin the publisher key with `rye trust pin <fingerprint>` instead.

**Controls:** `USER_SPACE` env var (overrides user root for testing)

**Log line for healthy boot:**
```
INFO ryeos_engine::trust: loaded trust store (operator-tier) count=2 dirs=1
```

---

## Phase 3: Bundle Root Resolution

Phase 1 of the two-phase node-config bootstrap determines which bundle roots the engine will scan. This happens in `BootstrapLoader::load_bundle_section()` (`ryeosd/src/node_config/loader.rs`).

**Scan sources** (the `bundles` section uses `SectionSourcePolicy::SystemAndState`):

1. `<system_data_dir>/.ai/node/bundles/` — system-level bundle registrations.
2. `<state_dir>/.ai/node/bundles/` — node-local bundle registrations.

Each `.yaml`/`.yml` file in these directories must:

- Be a regular file (symlinks are rejected).
- Have a valid `# rye:signed:...` signature header.
- Verify against the trust store (must reach `TrustClass::Trusted`).
- Declare `section: bundles` matching its parent directory name.
- Contain a `path:` field pointing to an existing directory (canonicalized).

After loading all records, **collision detection** runs: two records cannot share the same canonical path or the same name.

The resulting effective bundle roots are the `path` values from all verified records. These are passed to the engine builder in addition to `system_data_dir` itself (which is always included unconditionally).

**What can go wrong:**

- `node config item at <path> has no valid signature line` — the YAML is unsigned. Sign it with `rye sign <path>`.
- `node config item at <path> is not trusted (trust_class: Untrusted)` — the signer is not in the trust store. Pin the signer key.
- `bundle '<name>' path '<path>' does not exist or is not a directory` — the bundle root directory is missing.
- `node config section 'bundles' has duplicate name '<name>'` — two registrations with the same name.
- `node config section 'bundles' has duplicate canonical path '<path>'` — two registrations pointing to the same directory.

**Controls:** `RYE_SYSTEM_SPACE`, `--system_data_dir`, `--state_dir`

**Log line for healthy boot:**
```
INFO ryeosd::bootstrap: Phase 1: effective bundle roots determined system_data_dir=... bundle_count=1 trust_signers=2
```

---

## Phase 4: Engine Init

With the effective bundle roots in hand, `engine_init::build_engine()` (`ryeosd/src/engine_init.rs`) constructs the full `Engine`:

1. **Kind schemas** — loaded from `<root>/.ai/node/engine/kinds/*.kind-schema.yaml` across all system roots and user root. Each schema is verified against the trust store before admission.

2. **Parser tool descriptors** — loaded from the same search roots as kind schemas. Describes parser tools the engine can dispatch to.

3. **Handler descriptors** — loaded from all system roots and the user root (tier-tagged as `TrustedSystem` / `TrustedUser`). These define subprocess handlers for parsers, composers, and tools.

4. **Protocol descriptors** — loaded from the same tier-tagged roots. Defines wire contracts for subprocess communication (env vars, stdin framing, etc.).

5. **Composer registry** — derived data-drivenly from kind schemas (each kind declares its `composer:` handler ref).

6. **Boot validation** — cross-registry validation that every parser ref, composer ref, and protocol ref resolves correctly. All issues are collected and reported in a single error block.

7. **Protocol builder validation** — synthetic exercise of every protocol descriptor to catch regressions.

8. **Terminator→protocol ref validation** — every `Subprocess` terminator's `protocol_ref` must resolve in the protocol registry.

9. **Runtime registry** — built from verified `kind: runtime` YAMLs across all bundle roots. Fail-closed on verification errors or multi-default conflicts.

**Expected counts for a healthy boot** vary by installation, but all registries must be non-empty. A system with the standard bundle typically shows:

- Kind schemas: 5–15+
- Parser tools: 2–5+
- Handler descriptors: 3–8+
- Protocol descriptors: 2–4+
- Runtimes: 1+

**What can go wrong:**

- `no kind schema roots found; set system_data_dir or RYE_SYSTEM_SPACE` — the system data directory has no `.ai/node/engine/kinds/` subdirectory. Point `--system_data_dir` to a directory containing bundled kind schemas.
- `failed to load kind schemas` — a schema file failed verification (untrusted signer) or YAML parsing.
- `boot validation failed:` — one or more cross-registry issues (missing handler, unknown parser ref, etc.). Read the listed issues.
- `protocol builder validation failed:` — a protocol descriptor has invalid config (unknown env source, duplicate key, etc.).
- `kind '<name>' declares protocol '<ref>' but no such protocol is registered` — a kind schema references a protocol that doesn't exist in the protocol registry.
- `failed to build runtime registry` — a `kind: runtime` YAML is unsigned or untrusted.

**Controls:** `RYE_SYSTEM_SPACE`, `--system_data_dir`, `USER_SPACE`

**Log lines for healthy boot:**
```
INFO ryeos_engine::trust: loaded operator trust store count=2
INFO ryeosd::engine_init: loaded kind schemas count=12 roots=2 kinds=directive, graph, tool, service, runtime, ...
INFO ryeosd::engine_init: loaded parser tool descriptors count=3 duplicates=0
INFO ryeosd::engine_init: loaded handler descriptors count=5
INFO ryeosd::engine_init: loaded protocol descriptors count=3
INFO ryeosd::engine_init: loaded runtime registry count=2 roots=3
```

---

## Phase 5: Node Config Load

Phase 2 of the two-phase bootstrap loads all node-config sections via `BootstrapLoader::load_full()`. This is a full scan across all registered sections (`bundles`, `routes`, etc.) using their respective `SectionSourcePolicy`:

- **`bundles`** → `SystemAndState` (only `system_data_dir` + `state_dir`).
- **`routes`** → `EffectiveBundleRootsAndState` (`state_dir` + all bundle root paths).

Each YAML file is verified the same way as in Phase 3: valid signature header, trusted signer, `section` field matching parent directory.

The result is a `NodeConfigSnapshot` containing all bundle records and route specifications.

After the snapshot is built, the daemon constructs the **route table** from the route specifications. The route table maps incoming HTTP paths and webhook patterns to service handlers.

**What can go wrong:**

- Same verification errors as Phase 3 (unsigned, untrusted, wrong section, missing fields).
- `route table build failed at startup — check route YAML files` — a route spec has invalid fields, missing service refs, or other structural problems.

**Controls:** same as Phase 3

**Log lines for healthy boot:**
```
INFO ryeosd::bootstrap: Phase 2: node config loaded bundle_count=1 route_count=5
INFO ryeosd: route table built routes=5
```

---

## Phase 6: State Store + Lock

**State Store:**

The daemon initializes `StateStore` backed by SQLite (`<state_dir>/.ai/state/runtime.sqlite3`). The store manages thread state, event history, and CAS objects. It uses a `NodeIdentitySigner` (wrapping the node identity's Ed25519 key) for signing state writes.

**State Lock:**

Before accepting requests, the daemon acquires an exclusive `flock(LOCK_EX | LOCK_NB)` on `<state_dir>/.ai/state/operator.lock`. This prevents standalone services (`ryeosd run-service`) from running concurrently with the daemon. The lock is held for the daemon's lifetime and released on process exit (including panic).

**What can go wrong:**

- `StateStore initialization failed` — SQLite cannot open or create the database file. Check permissions on `<state_dir>/.ai/state/`.
- `failed to acquire state lock — is another ryeosd instance or standalone service running?` — another process holds the lock. Stop it first (the lock file contains the holder's PID).
- `failed to acquire state lock — is the daemon running?` — same condition from the standalone path.

**Controls:** `--db_path`, `--state_dir`

**Log lines for healthy boot:**
```
INFO ryeosd: StateStore initialized successfully
INFO ryeosd: State lock acquired
```

---

## Phase 7: HTTP + UDS Listeners

After all state is initialized and the operational service catalog self-check passes, the daemon:

1. **Reconciles threads** from the previous run — finds threads left in non-terminal states whose processes are dead, finalizes them as failed, and collects `ResumeIntent`s for threads that declare `native_resume` policy.

2. **Removes stale UDS socket** at the configured `uds_path` (if left over from a previous crash).

3. **Binds the TCP listener** on `config.bind` (default `127.0.0.1:7400`).

4. **Binds the Unix domain socket listener** on `config.uds_path`.

5. **Sets env vars** `RYEOSD_SOCKET_PATH` and `RYEOSD_URL` for subprocess discovery.

6. **Writes `daemon.json`** to `<state_dir>/daemon.json` — the daemon discovery contract containing PID, socket path, bind address, and start time.

7. **Sets UDS permissions** to `0600` (owner-only).

8. **Spawns the UDS serve task** and the **HTTP serve task** (with graceful shutdown via SIGINT/SIGTERM).

9. **Dispatches resume intents** — only after both accept loops are live, so resumed subprocesses can reach the daemon on their first callback.

**What can go wrong:**

- `failed to bind <addr>` — the TCP port is already in use. Change `--bind` or stop the conflicting process.
- `failed to bind <path>` — the UDS path is already in use and `remove_stale_socket` failed, or the parent directory permissions prevent socket creation.
- `failed to write daemon.json at <path>` — permissions issue on the state directory.
- `operational service catalog self-check failed: N service(s) failed verification` — an operational service failed resolve/verify/endpoint extraction.
- `operational service catalog self-check failed: N service(s) missing` — an operational service was not found in any bundle.

**Controls:** `--bind`, `--uds_path`, `--state_dir`

**Log lines for healthy boot:**
```
INFO ryeosd: StateStore initialized successfully
INFO ryeosd: State lock acquired
```

(The TCP/UDS binding and daemon.json writes produce no INFO-level logs on success — errors are the only logs in this phase.)

---

## What Gets Written

The daemon writes the following files and directories during its lifecycle:

**During init (Phase 1):**

| Path | Description |
|---|---|
| `<state_dir>/.ai/node/identity/private_key.pem` | Ed25519 node signing key |
| `<state_dir>/.ai/node/identity/public-identity.json` | Signed public identity document |
| `<state_dir>/.ai/node/vault/private_key.pem` | X25519 vault secret key |
| `<state_dir>/.ai/node/vault/public_key.pem` | X25519 vault public key |
| `<state_dir>/.ai/node/config.yaml` | Default daemon config (if missing) |
| `<state_dir>/.ai/node/auth/authorized_keys/` | Auth keys directory |
| `<state_dir>/.ai/state/objects/` | CAS object store |
| `<state_dir>/.ai/state/refs/` | CAS refs |
| `~/.ai/config/keys/signing/private_key.pem` | Ed25519 user signing key |
| `~/.ai/config/keys/trusted/<fp>.toml` | Self-signed trust entries (node + user) |

**During daemon startup (Phases 6–7):**

| Path | Description |
|---|---|
| `<state_dir>/.ai/state/runtime.sqlite3` | SQLite state database |
| `<state_dir>/.ai/state/operator.lock` | Exclusive state lock (PID written inside) |
| `<state_dir>/.ai/state/trace-events.ndjson` | Structured trace log |
| `<state_dir>/daemon.json` | Daemon discovery document (PID, socket, bind, start time) |
| `<uds_path>` | Unix domain socket (e.g. `$XDG_RUNTIME_DIR/ryeosd.sock`) |

**On shutdown, the daemon cleans up:**
- `daemon.json` is removed.
- The UDS socket file is removed.
- Running threads are drained (SIGTERM/SIGKILL by process group).

---

## Healthy Boot Log

The following shows the full sequence of INFO-level log lines from a successful `ryeosd` startup. Lines are ordered as they appear during the bootstrap:

```
INFO ryeosd::bootstrap: node signing key ready fingerprint=fp:a1b2c3... path=/home/user/.local/state/ryeosd/.ai/node/identity/private_key.pem
INFO ryeosd::bootstrap: user signing key ready fingerprint=fp:d4e5f6... path=/home/user/.ai/config/keys/signing/private_key.pem
INFO ryeosd::bootstrap: vault X25519 keypair ready fingerprint=fp:789abc... path=/home/user/.local/state/ryeosd/.ai/node/vault/private_key.pem
INFO ryeos_engine::trust: loaded operator trust store count=2 dirs=1
INFO ryeosd::bootstrap: Phase 1: effective bundle roots determined system_data_dir=/home/user/.local/share/ryeos bundle_count=0 trust_signers=2
INFO ryeos_engine::trust: loaded operator trust store count=2
INFO ryeosd::engine_init: loaded kind schemas count=8 roots=2 kinds=directive, graph, tool, service, runtime, node_config, kind_schema, parser_tool
INFO ryeosd::engine_init: loaded parser tool descriptors count=3 duplicates=0
INFO ryeosd::engine_init: loaded handler descriptors count=5
INFO ryeosd::engine_init: loaded protocol descriptors count=3
INFO ryeosd::engine_init: loaded runtime registry count=2 roots=2
INFO ryeosd::bootstrap: Phase 2: node config loaded bundle_count=0 route_count=5
INFO ryeosd: route table built routes=5
INFO ryeosd: StateStore initialized successfully
INFO ryeosd: State lock acquired
```

> **Note:** The trust store is loaded twice — once for Phase 1 bootstrap verification, and once inside `build_engine()` for full engine construction. This is by design: Phase 1 needs a trust store before the engine exists, and the engine needs its own copy for per-request verification.
