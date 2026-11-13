<!-- ryeos:signed:2026-11-13T02:46:08Z:90a251eaa2426c26d2a0fc8b17707a77e874e23000cb584eb83feb8281e466bc:lGshc9dxMXvNfrs65ZIALsG4Vg+K3PpqxSdBgdeo1z+9c1A4M5YhMbQEJueMBTXgfdg/mdkPJtiOIzPR5fLJAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
ryeos init --node-profile full --host-class local
ryeos start
ryeos node status
```

For packaged installs, `ryeos init --node-profile full --host-class local` is the required
setup command. It
uses `/usr/share/ryeos` as the default bundle source. Package install
hooks validate `/usr/share/ryeos/*/.ai` and print `Initialize with:
ryeos init --node-profile full --host-class local`.

## Lifecycle surface

The user lifecycle surface is exactly `ryeos init`, `ryeos start`,
`ryeos stop`, and `ryeos node status`. There is no restart, enable/disable,
init-system integration, or separate probe command. Lifecycle commands
are local-node operations and ignore `RYEOSD_URL`.

## What `ryeos init` does

`ryeos init` is implemented by `ryeos-node` and is authoritative for
operator-owned setup. It creates layout, user key, node key, self-trust,
official/additional publisher trust, discovers and plans bundles,
installs and registers bundles, creates vault key material, materializes the
selected complete policy generation, writes its read-only derived sync view,
and commits a signed whole-init fence over registrations and policy generation
before startup may proceed. The prior fence is durably invalidated before the
first mutation. Every distribution explicitly selects one complete
publisher-signed source-root init profile with `--node-profile <name>` and
one of its declared host classes with `--host-class <class>`.
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
├── graph/.ai/
└── not-a-bundle/
```

Immediate children containing `.ai/` are bundles. Hidden directories and
invalid names are skipped; bundle names are not hardcoded.

## Development setup

```bash
cargo build
./scripts/populate-bundles.sh --key .dev-keys/PUBLISHER_DEV.pem \
  --owner "RyeOS Development" --bundle-set full --all
ryeos init --source bundles --node-profile full --host-class local --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml
ryeos start
```

### A development node beside a real install

`scripts/pkg/install-local-direct.sh --prefix DIR` installs the packaged layout
under a separate root-owned prefix instead of `/usr`: binaries in `DIR/bin`,
bundle sources in `DIR/share/ryeos`. A prefix install must name its own node
with `--app-root` and refuses the default node at `~/.local/share/ryeos`, so
the real install and its node are never touched.

```bash
scripts/pkg/install-local-direct.sh --prefix /opt/ryeos-dev \
  --app-root "$HOME/.local/share/ryeos-dev" --trust-source-publishers
/opt/ryeos-dev/bin/ryeos node status --app-root "$HOME/.local/share/ryeos-dev"
```

- The prefix is off `PATH` by design. Call `DIR/bin/ryeos` by its absolute
  path; `ryeos start` finds its `ryeosd` beside it. The installer does not move
  `PATH` shadows or check `PATH` for a prefix install.
- Every existing directory from `/` down to the prefix must be owned by root
  and not writable by group or others. The installer creates `DIR` and
  `DIR/share` as root when they are missing, and refuses otherwise.
- `/usr/local` is refused because `/usr/local/bin` shadows `/usr/bin` on `PATH`.
- Init uses `DIR/share/ryeos` as its `--source`. Installs into the same prefix
  serialise on its package lock; different prefixes do not share a lock.

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

Remote CAS traversal and transfer capacity is likewise node-owned. The
mandatory `.ai/node/policies/object_closure.yaml` member supplies the exact
receive/serve ceilings used by handoff, federation, remote import, and local
admission. Callers may narrow those ceilings but cannot widen them; see
[Object Closure Policy](node/object-closure-policy.md).

For details, see [Local Node Lifecycle](node/lifecycle.md), [Operator
Init](node/operator-init.md), and [Identity Model](identity-model.md).
