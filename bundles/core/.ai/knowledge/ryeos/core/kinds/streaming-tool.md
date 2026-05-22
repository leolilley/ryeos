<!-- ryeos:signed:2026-05-22T04:30:07Z:617debe3f4e4bd7de361f63b9899ef8d02f8741d15bfe8344b36857f3d6a5e47:sNqKY81LQ/UMhUVIjQW4gWaSmzWfIN184DbYT4CVXkbuG0OUoKDeRvZxLSNWXl+Rdt0Xgj94trvmh/uNW1HbAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
