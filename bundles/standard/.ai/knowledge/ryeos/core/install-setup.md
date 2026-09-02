<!-- ryeos:signed:2026-09-02T08:05:36Z:6564b347a6f5ce478f13c30f90d9c21b1206439d3193611fd9f82310197d0161:3v/T0YBX33CnafPIpMC4OsV8Trcz5abuRTN4uvD9a3brLKoBy0K90q5j3E7swJ2cyyaJHA+30nYuWe0pnzfbBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [fundamentals, install, setup, init, bundles, getting-started]
version: "3.3.1"
description: >
  How to install and set up ryEOS from package to initialized local node.
  Covers ryeos-node init, bundle discovery, trust pinning, identity, and
  runtime startup.
---

# Installation and Setup

## Quick Start

```bash
# Install package, then initialize packaged bundles from /usr/share/ryeos
yay -S ryeos
ryeos init --node-profile full
ryeos start
ryeos node status
```

For packaged installs, `ryeos init --node-profile full` is the required
setup command. It
uses `/usr/share/ryeos` as the default bundle source. Package install
hooks validate `/usr/share/ryeos/*/.ai` and print `Initialize with:
ryeos init --node-profile full`.

## Lifecycle surface

The user lifecycle surface is exactly `ryeos init`, `ryeos start`,
`ryeos stop`, and `ryeos node status`. There is no restart, enable/disable,
init-system integration, or separate probe command. Lifecycle commands
are local-node operations and ignore `RYEOSD_URL`.

## What `ryeos init` does

`ryeos init` is implemented by `ryeos-node` and is authoritative for
operator-owned setup. It creates layout, user key, node key, self-trust,
official/additional publisher trust, discovers and plans bundles,
installs and registers bundles, creates vault key material, atomically
materializes the selected complete policy generation, writes its read-only
derived sync view, and verifies post-init trust. Every distribution explicitly selects one complete
publisher-signed source-root init profile with `--node-profile <name>`.
Selection verifies the profile's exact bundle inventory, then materializes the
complete generation beneath `.ai/node/policies/` under the node's own signer.
Bundle presence never selects policy implicitly, and fresh init refuses an
absent selection rather than inventing defaults.

Package reinstall and image restart use the mapped init profile only when
`.ai/node/policies/` is absent. Any present generation occupant causes
packaging to omit the selector and lets RyeOS validate and preserve the exact
node-signed generation. A partial, malformed, or unsafe occupant fails; it is
never replaced by the distribution profile.

Daemon bootstrap can repair daemon-local artifacts after init, but it
cannot install bundles or create operator trust artifacts and is not a
substitute for `ryeos init`.

## Bundle discovery

Source layout:

```text
source/
├── core/.ai/
├── standard/.ai/
└── not-a-bundle/
```

Immediate children containing `.ai/` are bundles. Hidden directories and
invalid names are skipped; bundle names are not hardcoded.

## Development setup

```bash
cargo build
./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem \
  --owner ryeos-dev --bundle-set full --all
ryeos init --source bundles --node-profile full --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml
ryeos start
```

## Directory layout after init/start

```text
<system-space>/.ai/config/keys/signing/private_key.pem
<system-space>/.ai/config/keys/trusted/<fp>.toml

<system-space>/.ai/bundles/<name>/.ai/
<system-space>/.ai/node/identity/private_key.pem
<system-space>/.ai/node/identity/public-identity.json
<system-space>/.ai/node/vault/{private_key.pem,public_key.pem}
<system-space>/.ai/node/auth/authorized_keys/<user>.toml
<system-space>/.ai/node/config.yaml
<system-space>/.ai/node/bundles/<name>.yaml
<system-space>/.ai/node/policies/<section>.yaml       # complete node-signed generation
<system-space>/.ai/node/sync/policy.yaml              # read-only derived view
<system-space>/.ai/state/{operator.lock,lifecycle-start.lock,runtime.sqlite3,scheduler.sqlite3,objects,refs}
<system-space>/daemon.json       # hint only while running
```

After `ryeos init`, `ryeos start` spawns `ryeosd`. The daemon verifies
initialization before writing runtime state, acquires the state lock
before unlinking sockets, repairs only daemon-local artifacts, then loads
registered bundles and starts listeners.

Bundle presence never selects an isolation backend. To enforce isolation,
install an independently authored backend bundle, apply a complete isolation
section through `ryeos node policy-apply isolation <source.yaml>` while the
daemon is stopped, validate with `ryeos node doctor`, and restart. See [Execution
Isolation](node/execution-isolation.md).

For details, see [Local Node Lifecycle](node/lifecycle.md), [Operator
Init](node/operator-init.md), and [Identity Model](identity-model.md).
