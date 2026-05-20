---
category: ryeos/core/kinds
tags: [kind, streaming-tool, subprocess]
version: "1.0.0"
description: Streaming tool kind reference.
---

# Kind: streaming_tool

Invariant: `streaming_tool` is a tool-like executable kind whose subprocess output is length-prefixed streaming frames instead of one opaque stdout blob.

- Directory: `tools/`
- Formats: same as `tool`
- Protocol: `protocol:ryeos/core/tool_streaming_v1`
- Composer: identity
- Alias: `@subprocess` → `tool:ryeos/core/subprocess/execute`

Use streaming tools when callers need incremental JSON events while the process is still running.
