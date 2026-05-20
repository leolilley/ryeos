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

Tool, streaming-tool, and runtime execution blocks refer to protocol refs such as `protocol:ryeos/core/opaque`, `protocol:ryeos/core/tool_streaming_v1`, and `protocol:ryeos/core/runtime_v1`.
