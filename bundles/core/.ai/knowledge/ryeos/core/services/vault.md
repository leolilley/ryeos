---
category: ryeos/core/services
tags: [service, vault, secrets]
version: "1.0.0"
description: Vault service reference.
---

# Services: vault

Invariant: vault services mutate or read sealed secrets scoped to the authenticated principal and daemon vault key material.

- `vault/set`
- `vault/list`
- `vault/delete`

These services are separate from runtime vault bindings: launch preflight resolves required secrets before spawning a subprocess.
