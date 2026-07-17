<!-- ryeos:signed:2026-07-15T07:49:19Z:2749f529f766d7e42ce022253c29dccd43782cc5952f6b9df8f4067cc33b1cbb:L7irEfiY8Rcpm26eJRfR3cwdit45DCrP3O5sPdqmz3W+7KIWoKrubebigX4b8JYP/ZXtMWFglhimhWl1iOP+Bg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
`protocol:ryeos/core/tool_callback`, which preserves this I/O shape but
explicitly declares callback authority and bindings.
