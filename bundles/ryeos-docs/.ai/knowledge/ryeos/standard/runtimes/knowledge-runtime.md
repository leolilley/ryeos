<!-- ryeos:signed:2026-11-13T00:55:33Z:2d5ea905e0459d38cf716f09cacc6e3eca812c099e8f70990b57b6d58772fe64:/Tn92sPtWHSpkudybnfJRlWFOhey1p43liFEvhfhOXr4RLYbzD2Qz1WX35vNPVjP3ZNZZSq9PLi3C8gZes2FCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

`validate` reports trust per item: an `unsigned` item is an error that makes
the corpus invalid, and an `untrusted_project` item is a warning naming it
(`untrusted signer in corpus: <ref> ...`). Admitting it stays the launch
gate's decision.

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
