<!-- ryeos:signed:2026-07-14T10:12:30Z:76529191f50ec8ac3815be79af8fd5f9cd985f2d27def9314cf4a0c29cde859b:dpwXlY94X19hRSU9sE6+mqh8q9OyTFHPrQ4X/WZWYkaHDXzIGOvSSjwaelDfZaKhFsie+3v5qAj3//1P+bLYBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/protocols
tags: [protocol, runtime-v1, callbacks]
version: "1.0.0"
description: Runtime v1 protocol reference.
---

# Protocol: runtime_v1

Invariant: `runtime_v1` launches a workflow runtime with a structured envelope, callback environment, thread id, vault bindings, and project context.

Directive and graph workflow runtimes use this protocol. Method runtimes such
as knowledge-runtime use the separate, schema-selected `method_runtime_v1`
contract (`MethodCallEnvelope` in and `MethodCallResult` out); the two wires are
not interchangeable, so a method-only runtime is not directly launchable as a
`runtime:` item through `runtime_v1`. Callback
authentication follows the UDS method access class: callback-token,
thread-auth, two-proof, chain-read, or exact-thread. Methods such as
`runtime.poll_input` and `runtime.author_item` require both callback capability
and thread-auth tokens; other methods enforce the narrower class assigned to
their handler. See `knowledge:ryeos/core/protocols/callback-auth`.
