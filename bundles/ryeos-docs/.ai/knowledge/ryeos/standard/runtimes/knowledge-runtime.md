<!-- ryeos:signed:2026-11-13T00:55:03Z:420e92801b6479f28157dd84b7cb3f3fe8c703df9fdd2e666df66703c0b5d35b:jOmBK+lSV/g6zOFCO+G/TKZQi9+M5kV7/xgNDCtuqHIX7m6IK3XG3QuI9AlKj2w8em0/kXK8neoNDoiHTMeUDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

Composed output renders each item under a `## Primary:`, `## Extends:` or
`## Reference:` header. An item signed by a pinned key renders with no
further mark. An `untrusted_project` item, which reaches composition only
when node `launch_trust` policy admits that class for the knowledge kind,
carries a trust note in its header, for example `## Reference: notes/x
(untrusted_project: signed by an unpinned key, admitted by node policy)`,
and its `resolved_items` entry reports `trust_class`.

The runtime registry selects this signed implementation binary for the
`knowledge` kind. The signed knowledge kind schema separately selects
`protocol:ryeos/core/method_runtime`, which carries a `MethodCallEnvelope`
on stdin and a terminal `MethodCallResult` on stdout with authenticated callback
bindings. Normal method calls and the `compose_positions` launch augmentation
share that wire. The binary is method-only and is not directly launchable as a
`runtime:` item through the unrelated `runtime` launch envelope.

The runtime attaches the child process and marks its thread running, then emits
the method result without self-finalizing. The daemon validates stdout and, for
augmentations, the derived parent projection before it publishes completed or
failed terminal state.
