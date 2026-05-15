---
category: ryeos/core
tags: [fundamentals, install, setup, init, bundles, getting-started]
version: "1.0.0"
description: >
  How to install and set up ryEOS — from package to running daemon.
  Covers init, bundle discovery, trust pinning, and the full directory
  layout after setup.
---

# Installation and Setup

## Quick Start

```bash
# 1. Install the package (Arch Linux example)
sudo pacman -U ryeos-0.5.0-1-x86_64.pkg.tar.zst

# 2. Initialize (discovers bundles from /usr/share/ryeos by default)
ryeos init

# 3. Start the daemon
ryeosd

# 4. Verify it's running
ryeos status
```

## What `ryeos init` Does

Init is a one-time setup that creates the full runtime environment. It's
**idempotent** — running it again is safe and preserves existing keys.

### Step by step

1. **Create directory layout** — system space, user space, CAS state,
   bundle dirs, vault, identity
2. **User signing key** — load-or-create at
   `~/.ai/config/keys/signing/private_key.pem`
3. **Node signing key** — load-or-create at
   `<system>/.ai/node/identity/private_key.pem`
4. **Self-trust** — both keys pinned into `~/.ai/config/keys/trusted/`
5. **Pin publisher key** — hardcoded official publisher Ed25519 key
   written to trust store (no on-disk trust needed)
6. **Pin additional trust files** — any `--trust-file` args processed
7. **Discover bundles** — scan `--source` for child dirs with `.ai/`
8. **Validate dependencies** — cross-bundle requires_kinds check
9. **Install each bundle** — preflight verification + copy + registration
10. **Vault keypair** — X25519 for sealed secrets (separate from identity)
11. **Post-init verification** — reload trust store, confirm all keys present

### Bundle discovery

Source layout (`/usr/share/ryeos` or custom `--source`):

```
source/
├── core/
│   └── .ai/          ← recognized as bundle "core"
├── standard/
│   └── .ai/          ← recognized as bundle "standard"
└── not-a-bundle/     ← no .ai/ → skipped
```

Bundle names are directory names. Rules: lowercase alphanumeric,
hyphens, underscores, 1–64 chars. Hidden directories (starting with `.`)
are skipped. There are no hardcoded bundle names — anything with `.ai/`
is installed.

### Preflight verification

Before copying a bundle, every signable item is checked:

- **Signed** — unsigned items are rejected
- **Signature valid** — Ed25519 verification against content hash
- **Signer trusted** — fingerprint must be in the operator trust store
- **Manifest valid** — if present, signature + identity + provides_kinds
  are verified

If any check fails, the entire bundle install is refused.

### Manifest dependency check

Bundles declare `requires_kinds` in their manifest. Init validates that
every required kind across all discovered bundles is provided by at
least one bundle's `provides_kinds`. If not, init fails with a clear
error listing which bundles need which missing kinds.

## Source Locations

| Scenario | `--source` | Notes |
|----------|-----------|-------|
| Packaged install | `/usr/share/ryeos` (default) | Zero flags needed |
| Development | `ryeos-bundles` | Add `--trust-file` for dev key |
| Docker | `/opt/ryeos` | |
| Custom | Any directory | Must contain bundle subdirs with `.ai/` |

## Development Setup

```bash
# Build
cargo build

# Populate bundles with signed items + staged binaries
./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev

# Init with dev keys
ryeos init --source ryeos-bundles --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml

# Run tests
cargo test
```

## Directory Layout After Init

```
~/.ai/
├── config/keys/
│   ├── signing/private_key.pem          ← your Ed25519 identity
│   └── trusted/
│       ├── <publisher-fp>.toml          ← official publisher trust
│       ├── <user-fp>.toml               ← self-trust
│       └── <node-fp>.toml               ← daemon trust

<system-space>/.ai/
├── node/
│   ├── identity/private_key.pem         ← daemon's Ed25519 key
│   ├── identity/public-identity.json    ← public identity doc
│   ├── vault/
│   │   ├── private_key.pem              ← X25519 sealed secrets key
│   │   └── public_key.pem
│   ├── auth/authorized_keys/
│   │   └── <user-fp>.toml               ← CLI auth (node-signed)
│   ├── config.yaml                      ← daemon config
│   ├── bundles/
│   │   ├── core.yaml                    ← signed registration records
│   │   └── standard.yaml
│   └── engine/kinds/                    ← merged kind schemas cache
├── bundles/
│   ├── core/.ai/                        ← installed core bundle
│   │   ├── manifest.source.yaml         ← hand-authored manifest
│   │   ├── manifest.yaml                ← generated + signed manifest
│   │   ├── handlers/ parsers/ services/ tools/
│   │   ├── node/engine/kinds/           ← kind schemas (9 kinds)
│   │   └── node/verbs/ node/aliases/
│   └── standard/.ai/                    ← installed standard bundle
│       ├── manifest.source.yaml
│       ├── manifest.yaml
│       ├── knowledge/                   ← directives, graphs, threads docs
│       └── node/engine/kinds/           ← kind schemas (3 kinds)
└── state/
    ├── objects/                         ← CAS object store
    └── refs/                            ← CAS refs
```

## Updating Bundles

Re-run `ryeos init` with the updated source. Bundles are atomically
replaced using stage → swap with one-generation backup. Keys, trust,
and registrations are preserved.

```bash
# After updating the package
ryeos init

# Or for development
./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev
ryeos init --source ryeos-bundles --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml
```

## Daemon Startup

When `ryeosd` starts:

1. **Verify initialized** — checks system space exists, at least one
   bundle registration present, keys exist
2. **Phase 1** — reads bundle registrations, discovers effective roots
3. **Build engine** — loads kind schemas, parsers, handlers from all
   registered bundles
4. **Phase 2** — full node-config scan across all sections
5. **Bind** — starts HTTP listener (default `127.0.0.1:9420`)

The daemon does NOT install or modify bundles — it only reads what
`ryeos init` wrote.
