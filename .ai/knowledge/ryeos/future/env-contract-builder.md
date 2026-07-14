<!-- ryeos:signed:2026-07-14T10:12:37Z:419300acda8215f63fc5cc0d97a8690c4061fde3d62fa003a528f533b03746f1:XhijM+SELODxJD8SxXDmzivfsM7EPaUVFWR6CNPioqLiwh5OhV63rHSdhlck/oLHyimpHoFPniVFDjawdOq6DQ==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
---
category: future
tags: [ryeos, security, env, secrets, architecture]
version: "1.0.0"
description: Implemented subprocess environment contract and its remaining follow-up work.
---

# EnvContractBuilder: implemented foundation and deferred follow-ups

## Current state

The central builder is implemented in
`crates/daemon/ryeos-app/src/env_contract.rs`. Subprocess launch paths compose
their final environment through typed source categories rather than overlaying
unclassified maps. It currently owns:

- the base host allowlist and daemon roots;
- declared and provider secrets;
- engine-plan, runtime-descriptor, interpreter, and path-mutation bindings;
- signed protocol injections with their exact `EnvInjectionSource` provenance;
- daemon resume and narrowly typed per-spawn bindings;
- protected-name validation, duplicate detection, and cross-source collision
  rejection.

Ordinary tools, managed runtimes, direct methods, compose augmentations, and
streaming subprocesses now carry verified protocol injections into this final
composition step. Callback tokens, thread-auth tokens, and daemon socket access
are produced only when the selected signed protocol declares the callback
channel and/or corresponding typed sources.
The default tool schema uses the explicit callback-capable
`tool_callback_v1`; callback-free protocols mint no callback authority.

## Established invariants

1. No subprocess receives blanket daemon host env.
2. Only explicitly declared or daemon-owned names are injected.
3. Application-controlled sources cannot override protected daemon, protocol,
   proxy, CA, or base-runtime names.
4. Protocol keys remain paired with compatible typed vocabulary sources.
5. Illegal collisions fail closed and diagnostics identify key and source without
   printing values.

## Deferred follow-ups

- Converge provider-secret and declared-secret resolution before construction so
  the two sources share one documented lookup policy as well as one collision
  policy.
- Finish routing the remaining validation/probe-only subprocess helpers through
  the same typed builder where they still use narrow wrappers.
- Converge the managed launch-envelope protocol builder with the common
  `SubprocessBuildRequest` vocabulary path once envelope serialization can be
  shared without losing exact signed-descriptor behavior.
- Decide whether ordinary executor-plan stdin needs a richer signed vocabulary.
  Today the protocol truthfully treats it as opaque: default wrappers normally
  serialize parameters as JSON, while executor runtime config materializes the
  final plan-owned bytes before protocol admission.
- Add structured, redacted diagnostics that can explain a rejected binding chain
  without exposing values.
- Keep `RYEOS_TOOL_ENV_PASSTHROUGH` limited to descriptor template
  interpolation; it is not a general secret or child-env inheritance mechanism.

These are cleanup and convergence tasks. The central security boundary itself
is present and should be extended rather than re-designed from the old proposed
API in this document's history.
