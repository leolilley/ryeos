<!-- ryeos:signed:2026-07-15T07:49:18Z:5eb204394603eaf875438ecb620071cf1d9c266522ef1a41ffebafc397e05811:4BIgVrqK4Egfy46AEaoKW1yY0yK1Ai28JS93eD+ek9dBIwvhFvKViKBo73UH8VtjbNcn7Ow/sEUW/3ahMve6Dw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/kinds
tags: [kind, protocol, subprocess]
version: "1.0.0"
description: Protocol kind reference.
---

# Kind: protocol

Invariant: `protocol` items describe subprocess wire contracts and are loaded as internal registry metadata.

- Directory: `protocols/`
- Formats: signed YAML
- Composer: identity
- Execution: none
- Required metadata: `name`, `category`, and `abi_version`

Tool, streaming-tool, and runtime execution blocks refer to protocol refs such
as `protocol:ryeos/core/tool_callback`,
`protocol:ryeos/core/tool_streaming`, and
`protocol:ryeos/core/runtime`. A method-bearing kind's
`execution.method_dispatch.protocol` selects a method wire such as
`protocol:ryeos/core/method_runtime`; the runtime registry selects only its
implementation binary. `protocol:ryeos/core/opaque` remains the explicit
callback-free terminal contract.
