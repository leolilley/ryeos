<!-- ryeos:signed:2026-07-14T10:12:30Z:669a0a1615c61a5bb832a31873c6b45924c40dca983e16b72f0542c7e019c8e0:A6Ycg3IAkj5Qurj7n3g2JW+zxluJ5Snkwx8os9SecYVV+sRuFw/Za0QayaQHgJRaXtQjrFSgc4nQDW/qN5EFAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/runtimes
tags: [runtime, knowledge, context]
version: "1.0.0"
description: Knowledge runtime reference.
---

# Runtime: knowledge-runtime

Invariant: knowledge-runtime composes knowledge entries into bounded context payloads for directives and explicit knowledge operations.

It serves the knowledge kind's schema-declared `compose`, `query`, `graph`, and
`validate` methods. It also implements `compose_positions` exclusively for the
daemon-owned `compose_context_positions` launch augmentation; that operation is
intentionally absent from the generic method map. The runtime applies bounded
budgets and exclusions and writes projection/chain data as required by each
operation's side-effect declaration.

The runtime registry selects this signed implementation binary for the
`knowledge` kind. The signed knowledge kind schema separately selects
`protocol:ryeos/core/method_runtime_v1`, which carries a `MethodCallEnvelope`
on stdin and a terminal `MethodCallResult` on stdout with authenticated callback
bindings. Normal method calls and the `compose_positions` launch augmentation
share that wire. The binary is method-only and is not directly launchable as a
`runtime:` item through the unrelated `runtime_v1` launch envelope.

The runtime attaches the child process and marks its thread running, then emits
the method result without self-finalizing. The daemon validates stdout and, for
augmentations, the derived parent projection before it publishes completed or
failed terminal state.
