<!-- ryeos:signed:2026-05-22T07:21:24Z:a484953a7ed86f0dd997ea35e5bf86dbe25cc48814f982d45de27684200c1c9c:K0eGskC0VG1hUd2CoPmrlet+HNZY7uYKeoiDLkd6I906TAhXrt2nPxbu0hzSxE4Agq5JkVnIUok9SOGs9+8zDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/handlers
tags: [handler, composer, identity]
version: "1.0.0"
description: Identity handler reference.
---

# Handler: identity

Invariant: the identity handler returns the parsed record unchanged and is used when a kind has no inheritance or multi-document composition semantics.

It is the default composer for config, handler, parser, protocol, runtime, service, node, tool, streaming_tool, and knowledge items. Validation remains the responsibility of the kind contract or the consumer-specific descriptor deserializer.
