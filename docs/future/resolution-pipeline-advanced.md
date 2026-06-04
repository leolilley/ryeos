# Advanced resolution pipeline

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
