<!-- ryeos:signed:2026-09-07T06:05:34Z:c377b84b83efbabc68e34f2f3b2f16084ed40b3f2ff4b386886af6c6237ddc4e:PtXrbGxJC+OIyXTBLZDE3tNEg3vyK/kkkaJa3xEXa/wi6bc2xmBRxjgxBYdVDtfAvkNTGXG144+H/TK+VxS8DA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/development
name: dependency-constitution
title: Workspace Dependency Constitution
description: Required dependency direction and layer ownership across the RyeOS workspace
entry_type: reference
version: "1.0.0"
```

# Workspace Dependency Constitution

RyeOS dependencies point from orchestration toward primitives. Lower layers
must not import authority, transport, or process orchestration from higher
layers.

The principal direction is:

```text
lillux
  ↑
state, bundle, engine, runtime protocols
  ↑
ryeos-app domain services
  ↑
executor
  ↑
API and node composition
  ↑
clients and binaries
```

`ryeos-app` is the daemon-independent domain/application-service layer despite
its historical name. The executor may consume those services; the application
layer must never depend back on the executor.

Rules:

- Workspace dependency cycles are forbidden.
- `lillux` owns low-level process, cryptographic, and durable-I/O primitives and
  imports no RyeOS layer.
- `ryeos-state` owns authoritative persistence and imports neither resolution
  nor orchestration.
- Engine and runtime protocol crates import neither daemon services nor
  execution orchestration.
- `ryeos-app` imports no executor, API, node-composition, or client crate.
- The executor imports no API, node-composition, or client crate.
- Shared types move downward only when they genuinely belong to the lower
  layer; forwarding modules and circular compatibility crates are forbidden.

`tool:ryeos/development/repository-validation/dependency-layers` enforces the cycle and forbidden-edge
portions directly from workspace manifests without invoking Cargo.

“Durable I/O” is defined by the platform-specific
[filesystem durability matrix](filesystem-durability.md). Higher layers own
multi-file reachability and crash recovery; a low-level atomic write does not
make a workflow a filesystem transaction.
