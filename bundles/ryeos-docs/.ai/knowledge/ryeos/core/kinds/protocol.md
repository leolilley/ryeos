<!-- ryeos:signed:2026-09-06T01:55:10Z:6a5e1ea59b61498e51e51b7e0a7bd5c01b9e6e3d029195372d6e393c392d988f:qsv6v3ZUxW834Awzp/opu6HbTpOhDyLjpxouzBnqIYw731aeYeJ/WJAlhMgkqZaexHgsGlDkoV1c2KaM4Q1tDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/kinds
tags: [kind, protocol, subprocess]
version: "1.1.0"
description: Protocol kind reference.
---

# Kind: protocol

Invariant: `protocol` items describe subprocess wire contracts and are loaded as internal registry metadata.

- Directory: `protocols/`
- Formats: signed YAML
- Composer: identity
- Execution: none
- Required metadata: `name`, `category`, and `abi_version`

Tool and runtime execution blocks refer to protocol refs such
as `protocol:ryeos/core/tool_callback`,
`protocol:ryeos/core/tool_streaming`, and
`protocol:ryeos/core/runtime`. A method-bearing kind's
`execution.method_dispatch.protocol` selects a method wire such as
`protocol:ryeos/core/method_runtime`; the runtime registry selects only its
implementation binary. `protocol:ryeos/core/opaque` remains the explicit
callback-free terminal contract.
