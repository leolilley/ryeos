<!-- ryeos:signed:2026-05-22T04:30:07Z:d1df765478e41beedcd4235ecef0168e8a92f2ea7e47829b206a5cd4423bb33c:4JNhlC7CMQvjVGkLQvru/coQnsjmvJJxBuRk1ZEDGwiedo3re/4mBNXf2PPh4c2gDwJ/gwgzVc/igopyLWenCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/kinds
tags: [kind, service, daemon]
version: "1.0.0"
description: Service kind reference.
---

# Kind: service

Invariant: `service` items are executable in-process daemon endpoints registered by endpoint name and gated by required capabilities.

- Directory: `services/`
- Formats: signed YAML
- Composer: identity
- Execution terminator: `in_process` registry `services`
- Metadata: `endpoint`, `required_caps`, `schema`, `description`

Services avoid subprocess overhead for daemon-internal operations such as thread queries, bundle management, object fetches, vault calls, and health/status checks.
