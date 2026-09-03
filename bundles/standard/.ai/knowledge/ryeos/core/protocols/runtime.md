<!-- ryeos:signed:2026-08-11T02:28:33Z:059e8f01daf80d561690895183d14e9d702e0661039c62b44f821c4732e2dc5b:u5kkAhlO9Ft1QXBrJjYB0eFpCD1g724nVgniFFvLyT/OmK00xtpQh/2rBC26CBbSRfecTiVfhzPSCmlxipjrAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

Managed runtime ABI v3 requires a finalized launch envelope. The envelope
contains the complete `ResolutionOutput` plus a required
`effective_definition_digest`; composition-capable runtimes recompute that
digest and execute `resolution.composed.composed`. The root source bytes are
retained only as provenance. Hook-capable kinds also require the captured
effective hook plan in the kind-declared derived slot. A missing plan, digest
mismatch, old ABI, or unknown envelope field fails before the first runtime
step.
