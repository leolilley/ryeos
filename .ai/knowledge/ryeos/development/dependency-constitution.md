<!-- ryeos:signed:2026-09-22T00:31:25Z:bd77d640ca6037afe47fe88d1f9cfcdf629a93f67787fd26abe46cb4b6a50baf:F3zJhJZXCQ1EYg3CUMYmI3q9+NNr33rWxGoOvrfeUQ+Qq3PnBTmLFuzylY78+eCpUi5RY2xixxrZZyNtsLgVCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/development
name: dependency-constitution
title: Workspace Dependency Constitution
description: Required dependency direction and layer ownership across the RyeOS workspace
entry_type: reference
version: "1.0.2"
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
  imports no RyeOS layer. It provides host execution, filesystem access,
  identity, clocks, waiting and communication mechanisms; this ownership is
  broader than security-sensitive operations. Portable standard-library APIs
  do not authorize a parallel implementation above that boundary.
- `ryeos-state` owns authoritative persistence and imports neither resolution
  nor orchestration.
- Engine and runtime protocol crates import neither daemon services nor
  execution orchestration.
- `ryeos-app` imports no executor, API, node-composition, or client crate.
- The executor imports no API, node-composition, or client crate.
- Shared types move downward only when they genuinely belong to the lower
  layer; forwarding modules and circular compatibility crates are forbidden.

Apply the boundary to complete capabilities, not lists of imports. RyeOS owns
protocols, policy, durable workflows and pure data processing over Lillux's host
primitives. Thread creation/joining and timed waits are host execution/time
operations; a duration value, shared reference or in-memory map is not a new
host capability. HTML handling, provider HTTP semantics and provider-specific
configuration stay with their feature owner. Third-party protocol libraries
still require review of their host I/O, timing and ambient configuration.

A small Lillux adapter is appropriate when it establishes a useful host contract.
Its size alone neither justifies nor disqualifies it. Prefer existing primitives,
state its exact guarantees, and distinguish local task completion from process
death, durable execution completion and filesystem writer exclusion. See the
[architecture map](architecture.md) for the corresponding implementation rules.

[Host Capability Ownership and Review](host-capability-boundary.md) governs
complete operation ownership and migration acceptance. Sharing host mechanisms
does not require identical public contracts for buffered and interactive
processes; preserve placement, cancellation and cleanup authority explicitly.
Dependency conformance alone does not prove that a consumer delegates host
operations correctly. The operation review and regression gates remain required.

`tool:ryeos/development/repository-validation/dependency-layers` enforces cycles
and forbidden owners across the complete production dependency closure directly
from workspace manifests without invoking Cargo. Diagnostics include the path
through intermediate crates. Target/build dependencies and inherited aliases
are included; development-only fixture dependencies are excluded.

“Durable I/O” is defined by the platform-specific
[filesystem durability matrix](filesystem-durability.md). Higher layers own
multi-file reachability and crash recovery; a low-level atomic write does not
make a workflow a filesystem transaction.
