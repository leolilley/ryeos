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
- Execution protocol: `protocol:ryeos/core/runtime_v1`
- Common fields: `serves`, `default`, `binary_ref`, `abi_version`, `required_caps`

Workflow kinds such as directive, graph, and knowledge delegate through the runtime registry; runtime descriptors select the concrete binary and ABI.
