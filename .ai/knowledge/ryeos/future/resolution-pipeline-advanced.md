<!-- ryeos:signed:2026-07-21T00:24:56Z:30cfeb2bc7699c5cae58b38a2a175215c3e319fc1c79d3a5afd2991cb82fdc63:AYSb5QONtT9VmXmIXKtl4ELJJ8B3k3vY3ubDc9Jdfuq9zyonMycyfP+NLDwrT2GkkD1KQWAfLt2oVSJHIrIGCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: resolution-pipeline-advanced
title: Advanced Resolution Pipeline
description: Deferred criteria for extending the closed resolution-stage registry
entry_type: design
version: "1.0.0"
```

# Advanced Resolution Pipeline

## Status

Deferred. The current kind dispatch model has a closed set of in-process
registries and subprocess terminators.

## Deferred work

Future resolution work may add additional in-process registries or resolver
stages, for example:

- parser registries;
- composer registries;
- richer protocol adaptation stages;
- multi-parent context resolution;
- non-service in-process handler families.

Do not add these until a concrete kind needs them. The current `services`
registry and subprocess terminator model is intentionally closed to keep the
dispatch path auditable.

## Requirements for any future expansion

1. The new stage must be represented in signed kind/node config.
2. The dispatch semantics must be deterministic and locally verifiable.
3. Preflight must reject unsupported stages rather than silently ignoring them.
4. Existing service/subprocess behavior must remain unchanged.
