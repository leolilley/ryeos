<!-- ryeos:signed:2026-07-21T00:24:55Z:d00aad6730438b4aa98020a3f15cb005393aa3639fd35a332b5c759ab4aa59f2:uXAdcqN20W0XPxtqdKveVSjpz0u2wW4Fx3hSmsIF5dt+iZ+LXuq9D/rvNERTYsRQW38fjZA2ZM2GL1bUtjREAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

`scripts/lint-dependency-layers.py` enforces the cycle and forbidden-edge
portions directly from workspace manifests without invoking Cargo.

“Durable I/O” is defined by the platform-specific
[filesystem durability matrix](filesystem-durability.md). Higher layers own
multi-file reachability and crash recovery; a low-level atomic write does not
make a workflow a filesystem transaction.
