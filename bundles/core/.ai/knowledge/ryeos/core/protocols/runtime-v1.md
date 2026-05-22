<!-- ryeos:signed:2026-05-22T03:35:36Z:4b93079ff3ed8296d460f7774cb36f26a575ec899b97982e271e92eb7ffd427f:DwiC+B5ooFoD08KYaqUu3nf+ceZBXmqzvMzwimlivcgxhqxRJSSUyFlmGzlSb29FIPWn6+XU3MPATTapAWoMCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/protocols
tags: [protocol, runtime-v1, callbacks]
version: "1.0.0"
description: Runtime v1 protocol reference.
---

# Protocol: runtime_v1

Invariant: `runtime_v1` launches a workflow runtime with a structured envelope, callback environment, thread id, vault bindings, and project context.

Directive, graph, and knowledge runtimes use this protocol. Runtime callbacks must authenticate with both callback capability and thread-auth tokens; see `knowledge:ryeos/core/protocols/callback-auth`.
