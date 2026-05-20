---
category: ryeos/core/handlers
tags: [handler, composer, identity]
version: "1.0.0"
description: Identity handler reference.
---

# Handler: identity

Invariant: the identity handler returns the parsed record unchanged and is used when a kind has no inheritance or multi-document composition semantics.

It is the default composer for config, handler, parser, protocol, runtime, service, node, tool, streaming_tool, and knowledge items. Validation remains the responsibility of the kind contract or the consumer-specific descriptor deserializer.
