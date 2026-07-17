<!-- ryeos:signed:2026-07-15T07:49:18Z:0bc74194b09f10e788352e4bcd47328192c5dec934dae225e890d99683e6308d:ogbbkreASIOnZJ21QOQLtlI7bG9SjE8zA5MPbQ+bH9b0hXu9bMbxgS+UrIU/Vtwu1wRyfUQyaD3ywOrGBHY0Ag==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
- Protocol: `protocol:ryeos/core/tool_streaming`
- Composer: identity
- Alias: `@subprocess` → `tool:ryeos/core/subprocess/execute`

Use streaming tools when callers need incremental JSON events while the process is still running.
