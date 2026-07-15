<!-- ryeos:signed:2026-07-14T10:12:30Z:8e9fda3deb8bb22b35f034c8ce19fd2b4382b23729b7d1e343a88f7c3f62e022:RvqxRlVt6FPVtSAzWt62FXAqgH8btRBtIkwjCyFb5ATkHks8Yv9DhEFfVszZ1Ex0pU+OyxGZTC0QIkMNmVE6Dg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
as `protocol:ryeos/core/tool_callback_v1`,
`protocol:ryeos/core/tool_streaming_v1`, and
`protocol:ryeos/core/runtime_v1`. A method-bearing kind's
`execution.method_dispatch.protocol` selects a method wire such as
`protocol:ryeos/core/method_runtime_v1`; the runtime registry selects only its
implementation binary. `protocol:ryeos/core/opaque` remains the explicit
callback-free terminal contract.
