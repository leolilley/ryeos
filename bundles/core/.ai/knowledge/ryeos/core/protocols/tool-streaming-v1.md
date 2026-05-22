<!-- ryeos:signed:2026-05-22T07:21:24Z:c29c3881c1f935b019d0cf58b0d574a60521b56ea4691669d81bfebf1618ce40:tfTSUIFINpPSB3Q4PWmjqtvIDiooS3N0QL7tNOD9oPGneS+zG1KdwKz7ePcgRJ4q43EIAsCCZElmmMuByDmnAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/protocols
tags: [protocol, streaming-tool]
version: "1.0.0"
description: Streaming tool protocol reference.
---

# Protocol: tool_streaming_v1

Invariant: `tool_streaming_v1` emits length-prefixed JSON frames so the daemon can stream tool progress before process exit.

The `streaming_tool` kind uses this protocol. It shares much of the tool runtime setup but changes stdout interpretation from opaque bytes to structured frames.
