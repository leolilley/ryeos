# Workspace dependency constitution

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
