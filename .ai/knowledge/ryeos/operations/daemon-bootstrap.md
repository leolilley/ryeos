---
category: "ryeos/operations"
name: "daemon-bootstrap"
description: "Every phase from binary launch to ready — config resolution, init, trust, bundles, engine, state, listeners"
---

# Daemon Bootstrap Sequence

The `ryeosd` daemon bootstrap is a multi-phase process that takes the binary from launch to "ready to serve requests." Each phase is fail-closed: any error prevents startup.

## Phase 0: Config Resolution

Config resolution happens in `Config::load()` (`ryeosd/src/config.rs`). The daemon merges values from CLI flags, env vars, config file, and compiled defaults.

**Key settings:**

| Key | Default | Override |
|---|---|---|
| `bind` | `127.0.0.1:7400` | `--bind` CLI flag |
| `state_dir` | `$XDG_STATE_DIR/ryeosd` | `--state_dir` |
| `system_data_dir` | `$XDG_DATA_DIR/ryeos` | `RYEOS_SYSTEM_SPACE_DIR` env or `--system_data_dir` |
| `uds_path` | `$XDG_RUNTIME_DIR/ryeosd.sock` | `--uds_path` |
| `db_path` | `<state_dir>/.ai/state/runtime.sqlite3` | `--db_path` |

If both CLI `--bind` and config file `bind` disagree, the daemon refuses to start unless `--force` is passed.

## Phase 1: Init (if needed)

If `--init-if-missing` is passed and keys are absent, `bootstrap::init()` creates:

1. **Directory layout**: `<state_dir>/.ai/node/{auth,vault}/`, `<state_dir>/.ai/state/{objects,refs}/`
2. **Default config**: `<state_dir>/.ai/node/config.yaml`
3. **Node signing key**: Ed25519 at `<state_dir>/.ai/node/identity/private_key.pem`
4. **User signing key**: Ed25519 at `~/.ai/config/keys/signing/private_key.pem`
5. **Public identity**: `<state_dir>/.ai/node/identity/public-identity.json`
6. **Vault keypair**: X25519 at `<state_dir>/.ai/node/vault/`
7. **Self-trust**: Trust entries at `~/.ai/config/keys/trusted/<fp>.toml`

## Phase 2: Trust Store Loading

`TrustStore::load_three_tier()` scans `~/.ai/config/keys/trusted/` for `.toml` files declaring trusted verifying keys.

## Phase 3: Bundle Root Resolution

Scans `<system_data_dir>/.ai/node/bundles/` and `<state_dir>/.ai/node/bundles/` for signed bundle registration YAMLs. Each must verify against the trust store.

## Phase 4: Engine Init

`engine_init::build_engine()` loads:
1. Kind schemas from `<root>/.ai/node/engine/kinds/*.kind-schema.yaml`
2. Parser tool descriptors
3. Handler descriptors
4. Protocol descriptors
5. Composer registry
6. Runtime registry

Cross-registry validation catches missing refs, unknown parsers, etc.

## Phase 5: Node Config Load

Full scan of registered sections (bundles, routes) using their `SectionSourcePolicy`. Builds the route table from route specifications.

## Phase 6: State Store + Lock

- SQLite state store at `<state_dir>/.ai/state/runtime.sqlite3`
- Exclusive `flock(LOCK_EX | LOCK_NB)` on `<state_dir>/.ai/state/operator.lock`

## Phase 7: Listeners

1. Reconciles threads from previous run
2. Removes stale UDS socket
3. Binds TCP on `config.bind`
4. Binds UDS on `config.uds_path`
5. Sets env vars `RYEOSD_SOCKET_PATH`, `RYEOSD_URL`
6. Writes `<state_dir>/daemon.json`
7. Spawns HTTP and UDS serve tasks
8. Dispatches resume intents

## Healthy boot log

```
INFO ryeosd::bootstrap: node signing key ready fingerprint=fp:... 
INFO ryeosd::bootstrap: user signing key ready fingerprint=fp:...
INFO ryeosd::bootstrap: vault X25519 keypair ready fingerprint=fp:...
INFO ryeos_engine::trust: loaded operator trust store count=2
INFO ryeosd::engine_init: loaded kind schemas count=8 kinds=directive,graph,tool,...
INFO ryeosd::engine_init: loaded parser tool descriptors count=3
INFO ryeosd::engine_init: loaded handler descriptors count=5
INFO ryeosd::engine_init: loaded protocol descriptors count=3
INFO ryeosd::engine_init: loaded runtime registry count=2
INFO ryeosd::bootstrap: Phase 2: node config loaded bundle_count=0 route_count=5
INFO ryeosd: route table built routes=5
INFO ryeosd: StateStore initialized successfully
INFO ryeosd: State lock acquired
```

## Files written

| Path | When | Purpose |
|---|---|---|
| `<state_dir>/.ai/node/identity/private_key.pem` | Init | Node Ed25519 key |
| `<state_dir>/.ai/node/identity/public-identity.json` | Init | Signed public identity |
| `<state_dir>/.ai/node/vault/{private_key,public_key}.pem` | Init | Vault X25519 keypair |
| `<state_dir>/.ai/node/config.yaml` | Init | Default config |
| `~/.ai/config/keys/signing/private_key.pem` | Init | User Ed25519 key |
| `~/.ai/config/keys/trusted/<fp>.toml` | Init | Self-trust entries |
| `<state_dir>/.ai/state/runtime.sqlite3` | Startup | State database |
| `<state_dir>/.ai/state/operator.lock` | Startup | Exclusive lock |
| `<state_dir>/daemon.json` | Startup | Daemon discovery |
| `$XDG_RUNTIME_DIR/ryeosd.sock` | Startup | UDS socket |

On shutdown: daemon.json and UDS socket are removed, running threads are drained.
