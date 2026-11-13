<!-- ryeos:signed:2026-11-13T00:27:41Z:309bcda30fcc169b6688e6f74a3622a672364dfc1681edcd9fa600aeb2f022fe:8nfw0euXjLzIdLqKLwQpdRzBsOiD+56VId846KEvKCXrSFUYypSc2fFx5KiFa96LBS+kg0Z+Z4dExwnhTSQBBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/protocols
tags: [protocol, method-runtime, callbacks, methods]
version: "1.1.0"
description: Method runtime protocol reference.
---

# Protocol: method_runtime

Invariant: `method_runtime` is the signed subprocess wire selected by a
method-bearing kind's `execution.method_dispatch.protocol`; the runtime
registry selects only the signed implementation binary.

The daemon sends a `MethodCallEnvelope` with `schema_version: 2`, containing
the resolved kind and method, verified method payload, bound arguments, runtime
configuration, the selected process workspace, child thread identity, and
callback capability. Durable project/state authority stays in the daemon and
is recovered from the callback token. The runtime returns one terminal
`MethodCallResult` and must echo
the dispatched kind and method.

The descriptor-aware producer and the runtime reject any other envelope schema
version; the field is an enforced wire discriminator, not advisory metadata.
The result is also semantically strict: success contains output and no error;
failure contains an error and no output. The runtime attaches and marks its
thread running, but the daemon owns terminal publication after it validates the
process outcome, result wire, echo fields, and any parent-view projection.

Accepted-launch preflight, boot validation, and live dispatch require the same
exact descriptor contract:
`method_call_envelope` stdin, terminal `method_call_result` stdout,
managed lifecycle, `runtime_socket` callbacks, and the canonical
`RYEOSD_THREAD_AUTH_TOKEN` binding from the `thread_auth_token` injection
source. Normal method dispatch and method-based launch
augmentations both use the descriptor-aware stdin builder and stdout decoder.

This is deliberately distinct from `runtime`, whose wire is
`LaunchEnvelope` to `RuntimeResult`. A runtime serving a
method-dispatch-only kind cannot be launched directly through that ordinary
runtime wire.
