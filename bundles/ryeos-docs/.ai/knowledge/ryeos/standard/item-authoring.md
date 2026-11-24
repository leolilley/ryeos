<!-- ryeos:signed:2026-11-22T00:24:31Z:82be60673049b9b96321e930288e7109ad4596e3521076142c94415eb69d2fcb:Z97AMNo/uuHnjXufZwXeU8zgLvs1RaBcp9y9P3Rz8QCgDjB3sGthFZNPutIHjt0MnJ0/FypReilkTtuTj7RRCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

## Wildcard semantics: fails closed, at mint and compose time

The pattern rule is subtle enough to state precisely. **Pattern grammar
exists only for `item_authoring` namespaces** — manifest validation rejects
`*`/`?` outright in `bundle_events.event_kind` and `runtime_vault.namespace`
(those two families are concrete-only on both sides, matched by exact id at
mint and compose time alike). For item-authoring namespaces, the same rule
applies in both places it matters — when the daemon mints a token from the
manifest, and when an `extends` chain composes a child's `requires` against
its parent's:

- **Concrete vs pattern — glob match.** A concrete request (no `*`/`?`) is
  backed by any declared pattern that matches it:
  `namespace: runtime-authored/foo` is backed by a declared
  `runtime-authored/*`.
- **Pattern vs pattern — identity only.** A request that itself carries a
  wildcard is backed **only** by an *identical* declaration. There is no
  containment analysis between two patterns: deciding whether one glob's
  match-set falls inside another's is exactly the kind of cleverness that
  ships an escalation, so the check refuses to try. `runtime-authored/*`
  requested against a declared `runtime-*/*` **fails**, even though every
  string the first matches, the second also matches.
- **Compose time is the same check.** In an `extends` chain, a child may
  *narrow* a parent's item-authoring pattern with a concrete value
  (`runtime-authored/foo` under a parent's `runtime-authored/*` composes
  fine). A child carrying a wildcard composes only when the parent declares
  the identical pattern; anything else is treated as widening and fails the
  composition — loudly, at signing/resolution, not at run time.

Practical consequence for authors: declare patterns in the manifest, request
**concrete** values in items wherever you can, and reuse the manifest's exact
spelling on the rare occasion an item genuinely needs the whole pattern. For
bundle events and the runtime vault, spell every event kind and namespace
out — a wildcard there is a validation error, not a shorthand.

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
- `mode` — `create` (default; fails if the item already exists), `upsert`
  (replaces the existing file, or creates it when there is none) or `remove`
  (removes a runtime-authored item; takes no `content`).
- `format_ext` — the file extension (e.g. `.md`); required when creating a new
  item.
- `expected_digest` — compare-and-swap guard for `upsert` and `remove`.
  **Required when the item already exists**: the write commits only if the incumbent's authored
  `content_digest` (from its provenance, and from the response that wrote it)
  equals this value, and fails with an `author_item conflict` otherwise. An
  upsert without it over an existing item is refused with code
  `author_item_expected_digest_required`, naming the incumbent's
  `content_digest` to resend with. An item with no runtime provenance (one the
  operator signed) cannot be replaced or removed through `author_item` at all;
  an operator-authored item is removed only with `ryeos item remove`, which the
  operator runs in their own process.

On success the daemon:

- authorizes `ryeos.author.<kind>.<bare-id>` against the callback token,
- derives the path from the kind schema's directory plus the bare id (rejecting
  symlinks and non-relative components),
- parses and path-anchors the body against the kind schema,
- checks the capability ceiling (below) before anything is signed,
- appends a signed `ryeos:authored:` provenance comment (author = runtime, plus
  thread, invocation, parent item ref, acting principal, effective bundle id,
  target ref, and content digest),
- signs with the daemon identity and writes atomically.

Response: `{ item_ref, path, mode, signer_fingerprint, content_digest,
required_capability, revision, revision_digest, previous_digest }`.

A kind whose signed schema declares `runtime_authoring: refused` (the `revision`
kind) cannot be written through the callback by any run, whatever it holds
(`author_item_kind_not_authorable`).

### Revision history

Every write and removal is also recorded as a **revision**: a signed record of
the core `revision` kind in the project, at
`.ai/revisions/<kind>/<bare-id>/<seq>.yaml`. The chain of one item is numbered
from 1; each record is signed by the key that made the change (the node's
authoring key for a runtime-authored change, the operator's key for one made
with `ryeos sign` or `ryeos item remove`), and carries the signed item bytes,
the digest of the previous record, the item file's digest before and after, and
the provenance (thread, run, acting principal, capability) and `recorded_at`,
all inside the signed body. A removal is a tombstone record with no
`after_digest`. The history travels with the project.

The chain is provenance and undo, never authority: the loader reads the file on
disk. A file whose digest is not the last record's `after_digest` is reported
as an `unrecorded_change`. The node also keeps a node-signed head per project
naming the newest record it has attested for each chain; a chain shorter than
or different from its head is reported as `truncated` or `diverged`, and
`author_item` refuses to extend it (`revision_chain_truncated`,
`revision_chain_diverged`). Read a chain with `service:revisions/list` and
`service:revisions/get`, which return each record with its verification.

v1 supports live-filesystem project provenance only.

### Announcing a change

Once a change is recorded, the node announces it. The announcement is the
`item_revised` event: `{ project, item_ref, digest, revision, change_set }`,
where `project` is the canonical root path of the project, `digest` is the
digest of the item file as of that revision (absent when the item was
removed), and `revision` is the item's position in its chain. The event names
no consumer and no kind. Its durable form is the revision record, which is
written first; the live form is a broadcast hub beside the thread event hub,
with the same capacity and the same lag rule. A subscriber that falls behind
loses broadcasts and recovers by re-reading the chain heads, so a lost
announcement is harmless: it only tells a reader to look again.

What announces a change:

- `author_item`, after the file and the record are durable (not when the run
  writes into a candidate integration workspace, which is not a live project).
- `ryeos sign`, `ryeos item remove` and `ryeos endorse`. These run in the
  operator's process with the operator's key, so after the change the CLI calls
  the node service `items/revised` with a request the operator signs like any
  other service call. The node does not take the caller's word: for a recorded
  change it verifies the item's revision chain (every signature, signer and
  link it has not already attested) and announces the newest record; for an
  endorsement it finds the endorsement in force in the ledger and announces the
  items it covers. A request the node cannot verify announces nothing. The
  call needs the exact capability `ryeos.execute.service.items/revised`, which
  no seat holds. When no node runs on this machine the CLI skips the call and
  says so in its output; the change itself is already made.
- Node startup. The node keeps, per project, the newest revision it has
  announced for each item. At startup it compares every chain head with that
  record and announces whatever is newer, so a change made while the node was
  stopped is announced once.

A session that holds an attachment naming a revised item is sent the
announcement as an `item.hint` on its session bus and swaps the recompiled
attachment into its open Surfaces in place; the root attachment is not swapped
(see the UI frame contract).

A change to the policy generation is not an item revision and is not announced
this way. Editing an item file by hand announces nothing: an unsigned edit is
not a change of record, and no file watcher exists.

### The capability ceiling

An authoring run cannot write an item that declares more authority than the run
itself holds. After the body is parsed and path-anchored, and before it is
signed, the daemon collects every capability the item would carry and requires
each to be covered (exact match or wildcard pattern) by the authoring run's
effective caps:

- the body's own `effective_caps` policy fact, at the path the kind schema
  declares for it (for directives and graphs, `requires.capabilities.declared`);
- when the body omits that declaration but `extends` a parent, the parent's
  composed `effective_caps` — an item that inherits a broad parent carries the
  broad parent's caps;
- every manifest runtime capability the body requests under
  `requires.capabilities.manifest.runtime_authority`.

Kinds without an `effective_caps` policy fact (knowledge, for instance) carry no
declared caps and are unaffected by the declared-caps part of the check. An
uncovered capability is refused with code `authored_capability_not_covered`,
naming the capability, the target ref and the authoring thread. The cure is in
the message: the authoring run must hold that capability, or the body must drop
it. An `extends` parent written as an `@alias` is refused too, because its caps
cannot be bounded without the canonical ref.

### Operator signature is the trusted path

The operator's signature (`ryeos sign`) is the trusted path for items. An item
the runtime authors is a machine draft: it is signed by the node's **authoring
key** (not the node key, not the operator key) and tagged with
`ryeos:authored:` provenance, and it stays a draft until the operator reviews
it and signs it again with their own key. Inspection (`authorship` in
`items.effective`) is how a reviewer tells the two apart.

### The authoring key and its scope

`ryeos init` generates the authoring key in its own key home,
`<system>/.ai/node/authoring/signing/private_key.pem`, and pins it in the signer
registry with role `authoring`, spaces `[project]` and **no kinds**
(`kinds: []`). The home sits under the live-execution floor, so a run never
reads or writes it. The daemon loads the key for each `runtime.author_item`
call and signs the item with it; the response's `signer_fingerprint` is the
authoring key's. If the key cannot be loaded the call is refused with
`author_item_authoring_key_unavailable`; the cure is `ryeos init` on the stopped
node, which creates and pins it. Nothing creates the key for an existing node
except re-running `ryeos init`.

Because the pinned scope admits no kind, an authored item is out of scope for
every kind, and that scope is the only one the key may hold. In project space an
out-of-scope item resolves as `untrusted_project` and is refused at launch; it
is a machine draft to read, not to run. An operator makes a draft trusted by
endorsing it, which leaves the item and its authoring signature untouched:

```bash
ryeos trust show <authoring-fingerprint>           # the record and its events
ryeos --project . endorse --item knowledge:notes/draft      # these exact bytes
ryeos --project . endorse --standing --kind knowledge       # the kind, in this project
ryeos endorse list
ryeos endorse revoke --endorsement <digest>
```

After a per-change endorsement the item resolves as `trusted_project` for those
exact bytes; one changed byte returns it to `untrusted_project`. A standing
endorsement covers the named kinds in that project and nothing else. Revoking
returns the covered items to `untrusted_project`. `ryeos trust scope` refuses to
widen the authoring key (`trust_authoring_scope_retired`). The node key does not
sign project items: its own scope is the node space only. The operator's
signature (`ryeos sign`) stays the trusted path for everything else. See
[Signing and Trust](../core/signing.md) for what an endorsement carries.

The authoring key rotates with `ryeos node authoring rotate` (stopped node):
the new key takes the authoring scope, the old key is retired and the items
it signed keep verifying; endorsements name digests and kinds, not keys, so they
keep covering them. See [Identity Model](../core/identity-model.md).

### How authored items resolve and show up

An authored item is signed by the **authoring** key. Whether it launches is the
registry scope's decision (above), not a property of the item: launch trust
does not tell an authored item from an operator-signed one once the scope
admits its kind. Inspection does: `items.effective` returns an `authorship` row
for the root, each extends ancestor and each referenced item, with
`signer_fingerprint`, `signer_role` (`authoring` for a key pinned with the
authoring role, `node` for this node's key, `other` for any other key, `none`
when unsigned) and `runtime_authored`, which is true only when an authoring key
signed the item and its signed bytes carry the `ryeos:authored:` provenance
line. A node-signed item with the line is not runtime-authored.

## Invoking it: the `author-item` tool

The callback is driven by the `ryeos-core-tools author-item` subcommand — a
capability-bounded callback client. When the daemon dispatches it as a tool it
reads the callback token, thread-auth token, and thread id from the env, sends
`runtime.author_item` with `{item_ref, content, mode, format_ext,
expected_digest}` on stdin, and
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
  input_data: "${params_json}"
  timeout_secs: 30
config_schema:
  type: object
  properties:
    item_ref:   { type: string }
    content:    { type: string }
    mode:       { type: string, enum: [create, upsert], default: create }
    format_ext: { type: string }
    expected_digest: { type: string }   # required by upsert over an existing item
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
