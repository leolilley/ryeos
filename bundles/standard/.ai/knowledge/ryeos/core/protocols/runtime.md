<!-- ryeos:signed:2026-07-15T07:49:19Z:f5fb14467d5c45926897bd20466614b70a9cd3dc006f817b43b40a09e2d05813:jTkNLWBX4FcsyYkUFF6SrBjYWxbD9R7eyh2PfCG0d8Ly24yDyxjf9bIdPgf6lSLdhpwxuGnbdgT7N059fjllDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/protocols
tags: [protocol, runtime-v1, callbacks]
version: "1.0.0"
description: Runtime v1 protocol reference.
---

# Protocol: runtime

Invariant: `runtime` launches a workflow runtime with a structured envelope, callback environment, thread id, vault bindings, and project context.

Directive and graph workflow runtimes use this protocol. Method runtimes such
as knowledge-runtime use the separate, schema-selected `method_runtime`
contract (`MethodCallEnvelope` in and `MethodCallResult` out); the two wires are
not interchangeable, so a method-only runtime is not directly launchable as a
`runtime:` item through `runtime`. Callback
authentication follows the UDS method access class: callback-token,
thread-auth, two-proof, chain-read, or exact-thread. Methods such as
`runtime.poll_input` and `runtime.author_item` require both callback capability
and thread-auth tokens; other methods enforce the narrower class assigned to
their handler. See `knowledge:ryeos/core/protocols/callback-auth`.
