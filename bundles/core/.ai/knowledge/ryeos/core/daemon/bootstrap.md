<!-- ryeos:signed:2026-05-22T03:35:35Z:30f279e792ed03c96aff8ee3f72f848284d93c386a1bf743cb61db0b6623f0a1:KOpT6hpcz9939pZKfB6z2MyTLfSPE+NMYBYeMptcqd9kk8XYNhJZ8/eHVthAp7HqG9qarRUIaDQujmHtmuzQCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/daemon
tags: [daemon, bootstrap, bundles, section-table]
version: "1.0.0"
description: >
  Daemon bootstrap order, raw YAML loading, and section table assembly.
---

# Daemon Bootstrap

Invariant: bootstrap loads enough signed raw YAML to construct the engine before asking the engine to parse higher-level items.

## Two-layer bootstrap

- **Layer 1 raw descriptors**: kind schemas, handler descriptors, parser descriptors, protocol descriptors, services, routes, verbs, aliases, and bundle registrations are read as signed YAML records.
- **Layer 2 engine items**: once registries exist, normal engine resolution can parse, compose, verify, and execute items by kind.

This split breaks the chicken-and-egg cycle: parser descriptors are needed to parse kind schemas, but kind schemas are also items.

## Section tables

The daemon's node loader merges `node/` sections from installed bundles and daemon state. Section paths are meaningful: `.ai/node/verbs/sign.yaml` must declare the `verbs` section, and route/alias/verb descriptors are registered into separate tables.

## Bundle contributions

Core contributes the engine machine and core node surface. Standard contributes workflow verbs, routes, handlers, runtimes, and workflow service descriptors. The loader treats both bundles as signed system-space contributors.
