<!-- ryeos:signed:2026-07-14T10:12:30Z:4f480ca766f974440bd65d9e6df9093cb10d9244d588bdd2a01da6a02deca770:dk5Zl7ZWGEBCNJ42GpXNvD2Feb2vUlIayT6O/FB3TcvGUJkcyPPpVbAht6Wa2tWti6GWvnyMrkxB5tNXnt+0Cw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/protocols
tags: [protocol, method-runtime-v1, callbacks, methods]
version: "1.0.0"
description: Method runtime v1 protocol reference.
---

# Protocol: method_runtime_v1

Invariant: `method_runtime_v1` is the signed subprocess wire selected by a
method-bearing kind's `execution.method_dispatch.protocol`; the runtime
registry selects only the signed implementation binary.

The daemon sends a `MethodCallEnvelope` with `schema_version: 1`, containing
the resolved kind and method, verified method payload, bound arguments, runtime
configuration, project/state roots, child thread identity, and callback
capability. The runtime returns one terminal `MethodCallResult` and must echo
the dispatched kind and method.

The descriptor-aware producer and the runtime reject any other envelope schema
version; the field is an enforced wire discriminator, not advisory metadata.
The result is also semantically strict: success contains output and no error;
failure contains an error and no output. The runtime attaches and marks its
thread running, but the daemon owns terminal publication after it validates the
process outcome, result wire, echo fields, and any parent-view projection.

Accepted-launch preflight, boot validation, and live dispatch require the same
exact descriptor contract:
`method_call_envelope_v1` stdin, terminal `method_call_result_v1` stdout,
managed lifecycle, `http_v1` callbacks, and the canonical
`RYEOSD_THREAD_AUTH_TOKEN` binding from the `thread_auth_token` injection
source. Normal method dispatch and method-based launch
augmentations both use the descriptor-aware stdin builder and stdout decoder.

This is deliberately distinct from `runtime_v1`, whose wire is
`LaunchEnvelope` to `RuntimeResult`. A runtime serving a
method-dispatch-only kind cannot be launched directly through that ordinary
runtime wire.
