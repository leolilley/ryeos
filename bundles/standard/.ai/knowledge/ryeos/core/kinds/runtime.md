<!-- ryeos:signed:2026-07-15T07:49:18Z:d04fd8924907b374efafd9f5eccad92f93c267c41a385344c454c87f6588d126:3GEdfk72SJ70AZtQJpqXIHy1qdLwxgKSXb/MWa6nfPfzepF7HZZHlbn5kMNoECJEk5dZiVYcUDyhophPVd4XBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/kinds
tags: [kind, runtime, delegate]
version: "1.0.0"
description: Runtime kind reference.
---

# Kind: runtime

Invariant: `runtime` items declare signed runtime binaries that serve delegated workflow kinds.

- Directory: `runtimes/`
- Formats: signed YAML
- Composer: identity
- Execution protocol: `protocol:ryeos/core/runtime`
- Common fields: `serves`, `default`, `binary_ref`, `abi_version`, `required_caps`

Workflow kinds such as directive, graph, and knowledge delegate through the runtime registry; runtime descriptors select the concrete binary and ABI.
