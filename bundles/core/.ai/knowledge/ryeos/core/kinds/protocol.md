<!-- ryeos:signed:2026-05-22T04:30:07Z:8e59338bc75e898c540868c965837bb53aaad04f5a6a5f673656ce4416638ea5:Gb1PNK/kGI46Yklzi0m+/hxljyIyxIwXKDeagnD4tDw3uJb8DIq6pKFGn7YDUBLL+J/AkyEGavll212gPZ8fBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
