---
category: ryeos/core/protocols
tags: [protocol, runtime-v1, callbacks]
version: "1.0.0"
description: Runtime v1 protocol reference.
---

# Protocol: runtime_v1

Invariant: `runtime_v1` launches a workflow runtime with a structured envelope, callback environment, thread id, vault bindings, and project context.

Directive, graph, and knowledge runtimes use this protocol. Runtime callbacks must authenticate with both callback capability and thread-auth tokens; see `knowledge:ryeos/core/protocols/callback-auth`.
