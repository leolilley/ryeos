# Quickstart

From source checkout to executing your first tool, step by step.

## Prerequisites

- Rust stable (1.80+)
- Linux (x86_64 or aarch64)

## Step 1: Build

```bash
cargo build
```

## Step 2: Initialize the node

This creates signing keys, trust store, and installs the core + standard bundles.

```bash
cargo run -p ryeos-cli -- \
  init \
  --core-source ryeos-bundles/core \
  --standard-source ryeos-bundles/standard
```

What this creates:
- Node identity key at `$XDG_STATE_DIR/ryeosd/.ai/node/identity/`
- User signing key at `~/.ai/config/keys/signing/`
- Core bundle (kind schemas, parsers, handlers) at `$XDG_DATA_DIR/ryeos/`
- Standard bundle (directives, tools, knowledge) registered in state
- Self-signed trust entries at `~/.ai/config/keys/trusted/`

## Step 3: Rebuild manifest (dev builds only)

After `cargo build`, binary hashes change. The daemon rejects hash mismatches. Fix:

```bash
cargo run -p rye-bundle-tool -- \
  rebuild-manifest \
  --source ryeos-bundles/core \
  --key ~/.local/state/ryeosd/.ai/node/identity/private_key.pem
```

## Step 4: Start the daemon

```bash
cargo run -p ryeosd --
```

The daemon binds to `127.0.0.1:7400` by default. It writes a discovery file at
`~/.local/state/ryeosd/daemon.json` with the bind address and socket path.

### Common startup issues

| Symptom | Fix |
|---------|-----|
| `HOSTNAME not set` | `export HOSTNAME=$(hostname)` |
| `no kind schema roots found` | Missing core bundle. Re-run `rye init --core-source` |
| `hash mismatch` | Run `rebuild-manifest` (Step 3) |
| `failed to acquire state lock` | Another `ryeosd` is running. Stop it first |

## Step 5: Verify

```bash
# Health check
curl http://127.0.0.1:7400/health

# Execute a built-in tool (no LLM provider needed)
cargo run -p ryeos-cli -- execute tool:rye/core/identity/public_key
```

## Step 6: Execute a directive

Directives need an LLM provider. Set the provider's API key:

```bash
export OPENAI_API_KEY=sk-...
# or
export ANTHROPIC_API_KEY=sk-ant-...
```

Then execute:

```bash
cargo run -p ryeos-cli -- execute directive:rye/core/init
```

## Development shortcuts

Add to your shell profile for convenience:

```bash
export PATH="$HOME/.local/share/cargo/bin:$PATH"

# After cargo build, link binaries somewhere convenient:
ln -sf $(pwd)/target/debug/ryeosd ~/.local/bin/
ln -sf $(pwd)/target/debug/rye ~/.local/bin/
ln -sf $(pwd)/target/debug/rye-bundle-tool ~/.local/bin/
```

## What's next

- [Installation](installation.md) — Full build and init reference
- [Daemon Bootstrap](../operations/daemon-bootstrap.md) — Every phase explained
- [Environment Variables](../reference/env-variables.md) — Complete env var reference
- [Dev Tree Caveats](../operations/dev-tree-caveats.md) — Working with dev builds
