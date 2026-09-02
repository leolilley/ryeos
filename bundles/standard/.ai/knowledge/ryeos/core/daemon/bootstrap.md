<!-- ryeos:signed:2026-09-02T10:47:09Z:7b329cd88fb608e33d4bddc991e14cbd9e637bce57199dc6c970f18cff51b211:sIx/I6064MYh+PW6gP4HbMEyHfIIS7O4SSADEneNyUdnKdQUMz01O+l/Y0SEhfzKyON19irlgsD7xCa8ztnEDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/daemon
tags: [daemon, bootstrap, bundles, section-table, repair, init]
version: "2.1.0"
description: >
  Daemon bootstrap order, operator-init vs daemon-repair boundaries,
  raw YAML loading, and section table assembly.
---

# Daemon Bootstrap

Invariant: `ryeos init` is authoritative for operator-owned setup;
`ryeosd` verifies that setup before writing runtime state, then repairs
only daemon-local artifacts.

## Operator init vs daemon repair

`ryeos init` owns user signing key, node signing key, user/node
self-trust docs, publisher trust pinning, bundle discovery/planning,
install, signed registrations, vault key creation, and post-init trust
verification. On first publication it selects one explicit signed init profile
and publishes that profile's complete policy set under `.ai/node/policies/`,
re-signed by the node. Runtime startup never fills omitted policy with defaults.

`bootstrap::repair_daemon_local` owns only daemon-local repair after
init-state verification. It first checks that operator signing key, node
signing key, operator trust doc, and node trust doc exist. Missing
artifacts fail with `Run: ryeos init` guidance. The daemon never writes
to operator trust and never regenerates the node key, because that would
invalidate the node trust doc in the node trust store.

Daemon-local artifacts repaired by startup include layout dirs, default
daemon config, public identity derived from node key, vault public/key
files, and the node-signed authorized-key entry for the local user key.
The trust directory is derived from resolved `config.user_signing_key_path`
layout `<user_root>/.ai/config/keys/{signing,trusted}/`, not by
re-reading `roots::user_root()`.

## Startup gate

`bootstrap::verify_initialized` uses `ryeos-node::require_initialized`.
Initialization requires at least one signed bundle registration in
`.ai/node/bundles/`; bundle names are not hardcoded. Direct `ryeosd`
startup on a fresh machine fails closed before tracing, socket cleanup,
runtime directory creation, or engine bootstrap. The removed `--init-only`
daemon path is not part of the system anymore.

Before runtime composition, the daemon strictly loads one complete atomic
node-policy generation. Missing, extra, malformed, or mixed-generation policy
fails startup. The compiled isolation member is passed into runtime resolution;
the engine never reopens a second raw policy path. Disabled mode does not inspect
a backend; enforced mode resolves the selected backend and resource limits before
listeners accept execution. Startup never rewrites policy authority.
See [Execution Isolation](../node/execution-isolation.md).

## Two-layer engine bootstrap

- **Layer 1 raw descriptors** — kind schemas, handler descriptors,
  parser descriptors, protocol descriptors, services, routes, verbs,
  aliases, and bundle registrations are read as signed YAML records.
- **Layer 2 engine items** — once registries exist, normal engine
  resolution can parse, compose, verify, and execute items by kind.

This split breaks the chicken-and-egg problem of parsers/handlers being
items needed to read other items.

## Section tables

The node loader merges `node/` sections from installed bundles and
daemon state. Section paths are meaningful: `.ai/node/commands/sign.yaml`
must declare `commands`, and route/command descriptors are registered
into separate section tables. Installed bundles are signed system-space
contributors.
