<!-- ryeos:signed:2026-05-22T07:21:24Z:617fc42bc0b7a6f9d21ae5b03cd522330c46a001a7ace444b3f96d9e72e4565d:klIGiQ2vkO5to2ydDq/WujImoxwpNw6AbTwX0m7G+B2v399XqBxu/fdziuiHaPAt7C4eUyP92+fUVMY6dMenCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
