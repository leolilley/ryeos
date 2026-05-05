# Installation

Build the Rye OS daemon and CLI from source.

## Prerequisites

- Rust toolchain (stable, 1.80+)
- `cargo`
- Linux (x86_64 or aarch64)
- Ed25519 signing support (via `lillux` — bundled)

## Build

```bash
cargo build
```

This produces:

| Binary            | Path                           | Purpose                 |
| ----------------- | ------------------------------ | ----------------------- |
| `ryeosd`          | `target/debug/ryeosd`          | The daemon              |
| `rye`             | `target/debug/rye`             | The CLI                 |
| `rye-bundle-tool` | `target/debug/rye-bundle-tool` | Bundle manifest tool    |
| `rye-sign`        | `target/debug/rye-sign`        | Item signing tool       |
| `rye-inspect`     | `target/debug/rye-inspect`     | Runtime inspection tool |

Release builds: `cargo build --release`.

## Initialize a node

`rye init` creates the node identity, user signing key, trust store, and installs the core + standard bundles.

```bash
# From the project root:
cargo run -p ryeos-cli -- \
  init \
  --core-source ryeos-bundles/core \
  --standard-source ryeos-bundles/standard
```

This creates:

| What             | Where                                         | Purpose                                     |
| ---------------- | --------------------------------------------- | ------------------------------------------- |
| Node signing key | `$XDG_STATE_DIR/ryeosd/.ai/node/identity/`    | Daemon's Ed25519 identity                   |
| User signing key | `~/.ai/config/keys/signing/`                  | Operator's Ed25519 identity                 |
| Vault keypair    | `$XDG_STATE_DIR/ryeosd/.ai/node/vault/`       | X25519 secret encryption                    |
| Core bundle      | `$XDG_DATA_DIR/ryeos/`                        | Kind schemas, parsers, handlers, runtimes   |
| Standard bundle  | `$XDG_STATE_DIR/ryeosd/.ai/bundles/standard/` | Directives, tools, knowledge                |
| Trust store      | `~/.ai/config/keys/trusted/`                  | Self-signed trust docs for node + user keys |

### Init flags

| Flag                       | Required | Description                                                                  |
| -------------------------- | -------- | ---------------------------------------------------------------------------- |
| `--core-source <PATH>`     | Yes      | Source tree to copy `core` bundle from                                       |
| `--standard-source <PATH>` | No       | Source tree to copy `standard` bundle from. Omit with `--core-only`          |
| `--core-only`              | No       | Skip installing the standard bundle                                          |
| `--state-dir <PATH>`       | No       | Override daemon state root (default: `$XDG_STATE_DIR/ryeosd`)                |
| `--user-root <PATH>`       | No       | Override user space root (default: `$HOME`)                                  |
| `--system-data-dir <PATH>` | No       | Override where the core bundle is installed (default: `$XDG_DATA_DIR/ryeos`) |
| `--force-node-key`         | No       | Force-regenerate the node signing key                                        |

## Required environment variables

The daemon **requires** `HOSTNAME` to be set. Most Linux desktops set this automatically; headless environments and containers may not.

```bash
export HOSTNAME=$(hostname)
```

See [Environment Variables Reference](../reference/env-variables.md) for the full list.

## Start the daemon

```bash
ryeosd
```

Or with explicit paths:

```bash
ryeosd \
  --state-dir $XDG_STATE_DIR/ryeosd \
  --system-data-dir $XDG_DATA_DIR/ryeos
```

The daemon writes `$STATE_DIR/daemon.json` with the bind address and socket path.

## Verify

```bash
# Health check
curl http://127.0.0.1:7400/health

# Execute a tool (no LLM provider needed)
rye execute tool:rye/core/identity/public_key
```

## Development builds and binary hashes

After `cargo build`, the binary hashes in the bundle manifest will be stale. The daemon rejects hash mismatches at runtime. Fix with:

```bash
rye-bundle-tool rebuild-manifest \
  --source ryeos-bundles/core \
  --key ~/.local/state/ryeosd/.ai/node/identity/private_key.pem
```

See [Dev Tree Caveats](../operations/dev-tree-caveats.md) for details.

## What's next

- [Quickstart](quickstart.md) — Execute your first directive and tool
- [Daemon Bootstrap](../operations/daemon-bootstrap.md) — Full bootstrap sequence explained
- [Environment Variables](../reference/env-variables.md) — Complete reference
