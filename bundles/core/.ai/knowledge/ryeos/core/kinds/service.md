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
