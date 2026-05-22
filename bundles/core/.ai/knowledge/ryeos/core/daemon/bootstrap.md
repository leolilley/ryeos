<!-- ryeos:signed:2026-05-22T07:21:23Z:c4bc11d2b84e87af0c994c1a12bc1c54acad40ffb84d70144f9dcfa6712662fc:FvHkSTOaDDVhtyx1Ga5zFiqfD5sXlzR9qe+92E9/y/bt3Rz4z0O2JblmDGx24EjbXTXBozzhJZEsCDlyu17YCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/daemon
tags: [daemon, bootstrap, bundles, section-table, repair, init]
version: "2.0.0"
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
verification.

`bootstrap::repair_daemon_local` owns only daemon-local repair after
init-state verification. It first checks that user signing key, node
signing key, user trust doc, and node trust doc exist. Missing artifacts
fail with `Run: ryeos init` guidance. The daemon never writes to user
trust and never regenerates the node key, because that would invalidate
the node trust doc in user space.

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
daemon state. Section paths are meaningful: `.ai/node/verbs/sign.yaml`
must declare `verbs`, and route/alias/verb descriptors are registered
into separate section tables. Installed bundles are signed system-space
contributors.
