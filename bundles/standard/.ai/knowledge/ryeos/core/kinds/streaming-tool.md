<!-- ryeos:signed:2026-08-11T02:28:31Z:f6b457f91a871183c85e42e9d9de0106f3b5ccd8fbc126299c9c9c5600a82192:euwJRQHZyyemKRs6oIq8JWSehj8UuHaxs+l0xHMKvWEi9L9S6v4lGe/9c8WCTky2Qar6gpEKqSWu7/f5wFsnBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

Streaming tools use the same adjacent-source contract as ordinary tools.
Publisher-owned source remains beside the item and is admitted before the
streaming subprocess receives authority; `external_content` remains reserved
for opaque content outside the authored item tree.
