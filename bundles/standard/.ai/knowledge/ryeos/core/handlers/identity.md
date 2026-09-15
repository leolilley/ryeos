<!-- ryeos:signed:2026-09-06T01:55:09Z:101a4cb738f7a4c83967611a29dd5c1b04d4b25ca0cb7e40bf2f0c5dec4fa6b7:Qn+NiAYd7qfMeXZAlZ/Dj238lRujv/saOoV7M7TTQfrtxlu9KHwpIXlEv9Sv3/5d/rMWYQovMTFB3B/wvEkaDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/handlers
tags: [handler, composer, identity]
version: "1.1.0"
description: Identity handler reference.
---

# Handler: identity

Invariant: the identity handler returns the parsed record unchanged and is used when a kind has no inheritance or multi-document composition semantics.

It is the default composer for config, handler, parser, protocol, runtime, service, node, tool, and knowledge items. Validation remains the responsibility of the kind contract or the consumer-specific descriptor deserializer.
