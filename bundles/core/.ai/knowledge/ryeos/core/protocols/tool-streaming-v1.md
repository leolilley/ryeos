---
category: ryeos/core/protocols
tags: [protocol, streaming-tool]
version: "1.0.0"
description: Streaming tool protocol reference.
---

# Protocol: tool_streaming_v1

Invariant: `tool_streaming_v1` emits length-prefixed JSON frames so the daemon can stream tool progress before process exit.

The `streaming_tool` kind uses this protocol. It shares much of the tool runtime setup but changes stdout interpretation from opaque bytes to structured frames.
