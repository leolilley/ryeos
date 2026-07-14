<!-- ryeos:signed:2026-07-14T10:12:30Z:c4249534aca5864fb9b3b36b417250f7a347689eae1152329b1193bf37a9b118:9VFZdJHmJ4HPGWMv+BXIMoXmyTMxPEwHtVWhvjh+5ROEkB4rF0AdH2VIprDxIThzIOMsEnXCHYKEECKhzBPHBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/protocols
tags: [protocol, opaque, tools]
version: "1.0.0"
description: Callback-free opaque terminal protocol reference.
---

# Protocol: opaque

Invariant: `opaque` is a callback-free terminal protocol: JSON input goes to the
subprocess and the process returns one opaque result after exit.

It is appropriate for a kind schema that needs neither incremental frames nor
daemon callbacks. The default `tool` kind instead selects
`protocol:ryeos/core/tool_callback_v1`, which preserves this I/O shape but
explicitly declares callback authority and bindings.
