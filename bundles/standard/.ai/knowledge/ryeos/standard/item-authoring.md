---
category: ryeos/standard
tags: [item-authoring, runtime-authority, manifest, capabilities, author_item]
version: "1.0.0"
description: >
  Runtime item authoring — how an executing item proposes a new signed project
  item through the daemon `runtime.author_item` callback, the authority that
  gates it, and the manifest declaration that backs it. Not to be confused with
  `directives/authoring.md`, which is about writing directive files.
---

# Runtime item authoring

Runtime item authoring lets an executing item (a runtime) propose the *bytes* of
a project item and have the daemon persist it as a normal signed project item.
The runtime never receives an item-signing key: the daemon authenticates the
callback, authorizes the target, derives the path from the kind schema, injects
signed provenance, signs with its own trusted identity, and writes atomically.

This is manifest-backed runtime authority — the same model bundle events and the
runtime vault use (`ryeos/standard/bundle-events`). Authority is always minted by
the daemon from a signed manifest, never self-granted by a running item.

## Capability shape

```
ryeos.author.<kind>.<bare-id>
```

- `<kind>` is the target item kind (e.g. `knowledge`).
- `<bare-id>` is the target's bare id / namespace (e.g. `runtime-authored/foo`).
- The daemon mints this only from a manifest-backed requirement; it can **never**
  be self-granted under `requires.capabilities.declared`.

## Declaring authority in the manifest

Declare the kinds and bare-id namespaces the bundle may author under
`runtime_authority` in the bundle's `.ai/manifest.source.yaml`, then sign the
bundle (`ryeos bundle publish`):

```yaml
name: arc
version: "0.1.0"
runtime_authority:
  item_authoring:
    - kind: knowledge
      namespace: runtime-authored/*
```

Namespaces are bare-id patterns; `*`/`?` are permitted and a request must fall
within a declared pattern.

## Requesting the subset an item needs

An item requests only the subset it needs; the daemon mints exactly that subset
into the callback token, and only where the signed manifest backs it:

```yaml
requires:
  capabilities:
    manifest:
      runtime_authority:
        item_authoring:
          - kind: knowledge
            namespace: runtime-authored/foo
```

A concrete request must fall under a declared pattern. A request that itself
carries a `*`/`?` wildcard must exactly match a declared pattern — it cannot
widen it.

## The `runtime.author_item` callback

Item authoring is a **daemon runtime callback** — not a tool ref, a service
item, or a directive. Because it is a durable signed project write it requires
**both** proofs a running runtime holds: the per-request `thread_auth_token` and
the exact-thread write-tier `callback_token`.

Request params:

- `thread_id` — the running thread (injected by the callback client).
- `item_ref` — the canonical target `kind:bare_id` (no ref suffix).
- `content` — the **unsigned** item body. It must not contain a signature line or
  a `ryeos:authored:` provenance marker; both are daemon-owned.
- `mode` — `create` (default; fails if the item already exists) or `upsert`
  (replaces the existing file).
- `format_ext` — the file extension (e.g. `.md`); required when creating a new
  item.

On success the daemon:

- authorizes `ryeos.author.<kind>.<bare-id>` against the callback token,
- derives the path from the kind schema's directory plus the bare id (rejecting
  symlinks and non-relative components),
- parses and path-anchors the body against the kind schema,
- appends a signed `ryeos:authored:` provenance comment (author = runtime, plus
  thread, invocation, parent item ref, acting principal, effective bundle id,
  target ref, and content digest),
- signs with the daemon identity and writes atomically.

Response: `{ item_ref, path, mode, signer_fingerprint, content_digest,
required_capability }`.

v1 supports live-filesystem project provenance only.

## Current invocation surface

The capability, the manifest declaration, the cap minting, and the daemon
`runtime.author_item` service are wired end to end. There is **not yet an
author-facing surface** that calls it: the callback client exposes
`CallbackClient::author_item`, but nothing in the runtime action surface invokes
it, and no tool, action verb, or language binding sends `runtime.author_item`.

To trigger authoring today a runtime must send the `runtime.author_item`
callback directly over the callback protocol with both proofs. A runtime-facing
surface (a tool or a graph/directive action verb that an item author can call) is
still to be built.
