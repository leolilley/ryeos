<!-- ryeos:signed:2026-07-15T07:49:19Z:e8a4155174d36cb7b54122738f2eff9ae99cf56c8264f370dc71b93f9d1ae93e:gv/Y7Ex+o0GypyMZWC4SO6VM7l6u/JBQkgnYJrjIeySPFb+5YIVdML/+MejomTWqqhYe9EGY2Nfhi/B8+YM2Bw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/protocols
tags: [protocol, streaming-tool]
version: "1.0.0"
description: Streaming tool protocol reference.
---

# Protocol: tool_streaming

Invariant: `tool_streaming` emits length-prefixed JSON frames so the daemon can stream tool progress before process exit.

The `streaming_tool` kind uses this protocol. It shares much of the tool runtime setup but changes stdout interpretation from opaque bytes to structured frames.
