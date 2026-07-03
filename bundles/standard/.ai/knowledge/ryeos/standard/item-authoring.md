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

## Invoking it: the `author-item` tool

The callback is driven by the `ryeos-core-tools author-item` subcommand — a
capability-bounded callback client. When the daemon dispatches it as a tool it
reads the callback token, thread-auth token, and thread id from the env, sends
`runtime.author_item` with `{item_ref, content, mode, format_ext}` on stdin, and
prints the daemon's response. It never touches project state directly.

### Authority lives on the wrapper tool — scope it tightly

A dispatched tool is minted its **own** callback token from its **own**
`requires`, backed by its **own** bundle's signed manifest. The caller's
authority does not constrain the target: the wrapper tool's
`runtime_authority.item_authoring` **is** the authoring authority.

> **Overgrant warning.** Any item that can `ryeos.execute.tool.<...>/author-item`
> can author anywhere in that wrapper's declared namespace — its own narrower
> `requires` will not limit `item_ref`. Granting execute on a broad authoring
> wrapper is equivalent to granting its full authoring namespace. Declare the
> narrowest `namespace` the wrapper is meant to expose, and prefer separate
> wrapper tools for distinct scopes over one wide wrapper.

### Where the wrapper lives — qualified binary reuse

Use `command: bin:core/ryeos-core-tools` from an authoring bundle wrapper. The
wrapper stays in the authoring bundle (so its own `requires` and signed manifest
own the authoring authority), while the executable is resolved from the signed
registered bundle whose manifest name is `core`.

Qualified binary refs are deliberately narrow:

- `bin:<name>` remains wrapper-bundle local.
- `bin:<bundle>/<name>` resolves only from registered signed bundle roots, never
  from project space or PATH.
- The source wrapper bundle must have a signed dependency relationship on the
  target bundle: one of its `requires_kinds` / `uses_kinds` must be provided by
  the target bundle. For `core`, wrappers that use tool items naturally require
  the `tool` kind.
- The target binary is still verified through the target bundle's binary
  manifest, CAS content hash, sidecar signature, trust store, and bundle
  confinement checks before dispatch.

### Wrapper tool item

Placed in the authoring bundle; its manifest must declare the matching
`runtime_authority.item_authoring`:

```yaml
# <authoring-bundle>/.ai/tools/<ns>/author-item.yaml
version: "0.1.0"
category: "<ns>"
name: "author-item"
executor_id: "@subprocess"
required_caps: ["ryeos.execute.tool.<ns>/author-item"]
description: "Author a signed project item via the daemon."
config:
  command: "bin:core/ryeos-core-tools"
  args: ["author-item", "--stdin-json"]
  input_data: "{params_json}"
  timeout_secs: 30
config_schema:
  type: object
  properties:
    item_ref:   { type: string }
    content:    { type: string }
    mode:       { type: string, enum: [create, upsert], default: create }
    format_ext: { type: string }
  required: [item_ref, content]
requires:
  capabilities:
    manifest:
      runtime_authority:
        item_authoring:
          - kind: knowledge
            namespace: runtime-authored/*   # narrow this to the exact subspace
```

The authoring bundle's `.ai/manifest.source.yaml` must both declare the matching
authority and depend on the bundle that provides the binary's kind surface, for
example (publish materializes `provides_kinds` into the generated
`.ai/manifest.yaml` — don't hand-maintain it):

```yaml
# <authoring-bundle>/.ai/manifest.source.yaml
name: <authoring-bundle>
version: "0.1.0"
requires_kinds:
  - tool
runtime_authority:
  item_authoring:
    - kind: knowledge
      namespace: runtime-authored/*
```

An executing directive or graph then calls the tool as an action with
`{item_ref, content, mode, format_ext}`, and the daemon writes the signed item.
