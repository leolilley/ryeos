<!-- ryeos:signed:2026-07-16T02:18:48Z:6b96f105729b7cfdfc0e7f2ba20f83ec208a10d0a690e7cc518f2aac23bda848:4RSLR9qwz0ziNUQC2iBmtLJPJ7+PQgur4/9s6Vl6ECumtE9u3N2U0EYo3dgZPmxpGvTePxyrmnQXlEKe9K00Cw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [fundamentals, install, setup, init, bundles, getting-started]
version: "3.1.0"
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
ryeos init
ryeos start
ryeos node status
```

For packaged installs, `ryeos init` is the required setup command. It
uses `/usr/share/ryeos` as the default bundle source. Package install
hooks validate `/usr/share/ryeos/*/.ai` and print `Initialize with:
ryeos init`.

## Lifecycle surface

The user lifecycle surface is exactly `ryeos init`, `ryeos start`,
`ryeos stop`, and `ryeos node status`. There is no restart, enable/disable,
init-system integration, or separate probe command. Lifecycle commands
are local-node operations and ignore `RYEOSD_URL`.

## What `ryeos init` does

`ryeos init` is implemented by `ryeos-node` and is authoritative for
operator-owned setup. It creates layout, user key, node key, self-trust,
official/additional publisher trust, discovers and plans bundles,
installs and registers bundles, creates vault key material, writes
create-once node policies (including the disabled strict sandbox policy), and
verifies post-init trust.

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
./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem --owner ryeos-dev
ryeos init --source bundles --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml
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
<system-space>/.ai/node/isolation.yaml
<system-space>/.ai/node/bundles/<name>.yaml
<system-space>/.ai/node/ingest/ignore.yaml
<system-space>/.ai/state/{operator.lock,lifecycle-start.lock,runtime.sqlite3,scheduler.sqlite3,objects,refs}
<system-space>/daemon.json       # hint only while running
```

After `ryeos init`, `ryeos start` spawns `ryeosd`. The daemon verifies
initialization before writing runtime state, acquires the state lock
before unlinking sockets, repairs only daemon-local artifacts, then loads
registered bundles and starts listeners.

Bubblewrap is optional while the sandbox policy remains in its default
`mode: disabled`. Install it before selecting `mode: enforce`, validate with
`ryeos node doctor`, and restart. See [Execution
Sandbox](node/execution-isolation.md).

For details, see [Local Node Lifecycle](node/lifecycle.md), [Operator
Init](node/operator-init.md), and [Identity Model](identity-model.md).
