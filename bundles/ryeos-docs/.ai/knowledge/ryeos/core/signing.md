<!-- ryeos:signed:2026-11-13T20:02:48Z:4bc3d2e5b2682863d2fed4c05a1c2446b8e05f671b01f13f7becb68bc646c221:Qm0zITcKjF/uhHwsEmZ5gyqmzSGQ3ICqGxjdj+Ce+p6hFNIfUpjhJ2YgBY0B+AnUOzZ6FVz7MxQYNkpoxovQCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
category: ryeos/core
tags: [fundamentals, signing, trust, integrity, compilation]
version: "3.2.0"
description: >
  How signing and trust work in Rye OS — Ed25519 signatures,
  content hashing, path anchoring, the trust store, the signed
  envelope format, and signing as a compilation step.
---

# Signing and Trust

Rye OS uses Ed25519 cryptographic signatures to ensure item integrity.
Every executable item must be signed before the daemon will run it.

## Four Keys, Four Roles

| Key | Purpose | Location |
|-----|---------|----------|
| **Operator key** | Operator identity — CLI auth, signing project items | `<system>/.ai/node/operator/signing/private_key.pem` |
| **Node key** | Daemon identity — bundle registrations, node config | `<system>/.ai/node/identity/private_key.pem` |
| **Authoring key** | Items the runtime authors; machine drafts, no kind admitted until the operator widens its scope | `<system>/.ai/node/authoring/signing/private_key.pem` |
| **Publisher key** | Bundle author identity — signs items in published bundles | Signer record in `<system>/.ai/node/trust/signers/` |

The operator, authoring and node keys are generated at `ryeos init` time.
Operator and authoring keys rotate with `ryeos node operator rotate` and
`ryeos node authoring rotate`; the node key does not rotate. The
publisher key is **hardcoded in the binary** and pinned during init
without trusting any on-disk file. For development, `--trust-file`
pins a dev publisher key instead.

## The Signed Envelope Format

Every item carries a signature in a comment header. The format is
four colon-delimited fields:

```
```

| Field | Format | Example |
|---|---|---|
| `timestamp` | ISO 8601 UTC | `2026-05-20T05:57:09Z` |
| `content_hash` | SHA-256 hex (64 chars) | `fb60141f8b9e...` |
| `signature` | Ed25519 base64url-padded | `mjlXVP85DZLrMf...` |
| `fingerprint` | SHA-256 of the public key (64 chars) | `741a8bc609b3...` |

The envelope prefix and suffix vary by file type:

| File type | Prefix | Suffix |
|---|---|---|
| YAML (`*.yaml`) | `#` | (none) |
| Markdown (`*.md`) | `<!--` | `-->` |
| Other | Configurable via `SignatureEnvelope` | (none) |

The signed content is every byte of the file except the signature line
itself. For scripts with a shebang (`.py`, `.sh`), the signature line
sits on the second line, below the shebang, and the shebang is signed
like the rest of the file: changing the interpreter line after signing
breaks the signature.

### Concrete examples

YAML:
```
```

Markdown:
```
```

The Ed25519 signature covers `SHA256(body)` — the hash bytes, not the
body bytes directly. Verification accepts standard base64, URL-safe-no-pad
base64, and URL-safe-padded base64 for the signature.

## How Signing Works

When you run `ryeos sign <ref>`:

1. The engine resolves the item by canonical ref
2. The file content (below the signature line) is hashed (SHA-256)
3. The hash is signed with the operator's Ed25519 private key
4. The signature line is written as the first line of the file

The signature covers:
- The **content hash** — any change to the file body invalidates the sig
- The **path anchor** — moving the file to a different path invalidates the sig

This means signatures are **tamper-evident** and **location-bound**.

## When to Sign

Sign after creating or editing any item:
- `ryeos sign directive:my/workflow`
- `ryeos sign tool:my/helper`
- `ryeos sign knowledge:my/context`
- `ryeos sign directive:*` (batch sign all directives)
- `ryeos sign tool:my/project/*` (namespace glob)

## Verification

`ryeos verify <ref-or-.ai-path> [<ref-or-.ai-path>...]` checks each target's:
1. The signature header exists
2. The content hash matches the current file content
3. The signing key is in the trust store
4. The path anchor matches the item's actual location

Unsigned or modified items produce an error with a clear
message telling you exactly what to fix.

## What Launches

Verification tells you an item's trust class. Launching decides whether
that class may run:

| Trust class | Meaning | Launches by default |
|---|---|---|
| `trusted_bundle` | Signed by a pinned publisher key, from an installed bundle | Yes |
| `trusted_project` | Signed by a pinned key, from the project | Where the kind admits projects |
| `untrusted_project` | Signed, but by a key that is not pinned | **No** — only if node policy admits it |
| `unsigned` | No signature, or one that does not verify | **Never** |

The rule holds on every path that runs something: direct tools, executor
hops, in-process services, method dispatch (knowledge), compose, managed
launches (directives, graphs), schedules and recovery. It judges the
launch root, each item it extends, and each item it references (for
example knowledge pulled in through `references:`), every one against its
own kind's lists: a pinned root cannot bring in an item its kind would not
run. A refusal is a 403 with code `effective_trust_unsigned` or
`effective_trust_refused`, naming the item, the class it saw and the
classes admitted.

**Each kind declares what it admits.** Every root-executable kind schema
carries a signed `launch_trust` block:

| Kinds | Spaces | Admitted by default | Operator may add |
|---|---|---|---|
| tool, directive, graph, knowledge | bundle, project | `trusted_bundle`, `trusted_project` | `untrusted_project` |
| service, client, runtime, worker_execution | bundle | `trusted_bundle` | nothing |

An item found in a space its kind does not admit is never a candidate: a
project file named like a bundle service is ignored (and reported as
shadowed), and the bundle service still answers. A service also runs only
its compiled handler, under the capabilities its compiled descriptor
requires, whatever the item names.

**Admitting an unpinned signer is the operator's explicit choice.** The
node policy section `launch_trust` (node-signed, under
`<system>/.ai/node/policies/`) admits classes per kind, and only those the
kind allows:

```yaml
schema: 1
admit:
  tool: [untrusted_project]
```

Apply it to a stopped node with `ryeos node policy-apply launch_trust
<file>`. Admission is by class, not by signer: admitting
`untrusted_project` for a kind admits every item of that kind signed by
any key, in every project on the node. Withdrawing it also stops work
admitted under it from resuming.

**Projects cannot pin keys.** Trust is held only by the node, in
node-signed signer records. Nothing a project contains, and nothing a live
run writes, can make a signer trusted. Live runs may write `.ai/tools/` and
`.ai/services/`: an item planted there is inert unless it is signed by a
key the node has pinned, for that project.

**Bundle-owned namespaces cannot be shadowed.** A bundle's signed manifest
can declare `owns_namespaces` (core owns `ryeos/core`). A project file inside
an owned namespace, such as a copy of a core runtime at
`.ai/tools/ryeos/core/runtimes/python/function.yaml`, is refused with a 409
`reserved_namespace_shadow` instead of winning resolution, even when a pinned
key signed it. See [Spaces](spaces.md).

**One name, one item.** Two files with the same name and different
formats in one folder (for example `x.py` and `x.yaml`) are refused as
ambiguous rather than one silently winning. A launch refuses with 409
`ambiguous_item_formats`, naming the item, the space and both candidate
files; the cure is to keep exactly one of them.

## Trust Store

Trust lives only on the node, as a registry of signer records in
`<system>/.ai/node/trust/signers/<fp>.yaml`. Each record is signed by the
node key; nothing writes trust by hand. The node key itself is the
implicit root: it is trusted for the node space only and needs no
record. It does not sign project items.

A record carries the signer's fingerprint and public key, a label, and:

| Field | Values |
|---|---|
| `role` | `publisher`, `operator`, `project_author`, `authoring` |
| `scope` | `spaces` (bundle, project, node), `kinds` (item kinds, or all), `projects` (project roots, for `project_author`) |
| `status` | `active`, `retired`, `revoked` |

Defaults: a publisher is scoped to the bundle space for every kind; the
operator key is scoped to the project and node spaces for every kind; the
authoring key is scoped to the project space with **no kinds** (`kinds: []`
admits nothing), so items it signs are untrusted until the operator widens the
scope with `ryeos trust scope <fp> --kind <kind>`.

`scope.projects` names projects by their logical project path: the absolute
project path as the node knows it. A run against a live project matches by that
path. A run against a pinned generation, a copy-on-write workspace or a pushed
head is materialized at a per-generation checkout, so it matches by the
project's original path instead, but only when the node's stable project
identity for it is local (`local:<path>`); the checkout path never matches. A
run whose logical project is unknown (for example a head whose project
identity names another site) is judged as no project, so a signer scoped to
specific projects is untrusted there.

The scope is checked on every verification. A signer outside its scope is
not trusted for that item: in project space the item is
`untrusted_project`; in bundle or node space the verification is refused
with `signer_out_of_scope`. A revoked signer verifies nothing
(`trust_signer_revoked`). Node documents (policy generations, bindings,
node configuration) verify only under the node key, never against the
registry.

Each record is checked when the registry loads. A record that fails is
refused with a typed code: `trust_record_unsigned`,
`trust_record_wrong_signer`, `trust_record_tampered`,
`trust_record_name_mismatch` (the file name is not the fingerprint),
`trust_record_schema_unsupported`.

During `ryeos init` the publisher key (hardcoded official key, or each
`--trust-file`) is pinned as role `publisher`, scope bundle.

**Pinned is not the same as allowed to sign node state.** Item
verification accepts any pinned key within its scope, but node state does
not: node policy, schedules, bundle registrations and app-root node config
verify only under the node key, and the init completion record only under
the local operator key. Anything else is refused as
`node_document_wrong_signer`. See [Identity Model](identity-model.md#who-signs-node-state).

**Bundles do NOT ship trust records.** The operator must pin publisher
keys explicitly via `ryeos init` (official) or `ryeos trust pin`
(third-party). This prevents a bundle from injecting its own trust.

### Changing trust

`ryeos trust pin --from <PUBLISHER_TRUST.toml> --role publisher` pins an
additional publisher; the other verbs change scope or status. See the
verb table in the CLI reference. The mutating verbs are offline
operations: stop the node first. Read-only `ryeos trust list` and
`ryeos trust show <fp>` work on a running or stopped node.

### Retired and revoked

A **retired** signer still verifies the items it already signed. A
**revoked** signer verifies nothing, and revocation is terminal: a
revoked key cannot be pinned again.

### Key events

Every trust change appends a node-signed, hash-chained key event under
`<system>/.ai/node/trust/events/`. Event fields:

| Field | Meaning |
|---|---|
| `schema` | `ryeos/trust-event/v1` |
| `seq` | position in the chain |
| `previous` | digest of the previous event |
| `kind` | `pinned`, `scope_changed`, `retired`, `revoked`, `succeeded`, `init` |
| `signer` | fingerprint of the key the event concerns |
| `record_digest` | digest of the resulting signer record |
| `request_digest` | digest of the signed operation that caused it |
| `operator` | the operator that requested the change |

Read them with `ryeos trust show <fp>`.

## Bundle Install Verification

When `ryeos init` installs a bundle (or `ryeos bundle install` is used),
every signable item is verified:

1. **Signature present** — unsigned items in a bundle are rejected
2. **Signature valid** — Ed25519 verification against content hash
3. **Signer trusted** — the signer's fingerprint must be in the
   operator's trust store
4. **Path anchored** — the item must be at its declared location
5. **Manifest verified** — if a signed manifest.yaml exists, its
   signature, identity, and provides_kinds are validated

If any check fails, the entire bundle install is refused with a clear
error listing every failed item.

## System Space Is Immutable

Bundle items (system space) cannot be signed with the node key — they
carry the publisher's signature. If you need to customize a system item,
copy it to project space first, then sign with your key.

## Key Fingerprints

The key fingerprint is the SHA-256 hash of the raw 32-byte public key,
hex-encoded (64 characters). It appears in:
- Signature headers (last field after the colon)
- Trust store filenames (`<fingerprint>.toml`)
- `ryeos identity` output

## Vault (Sealed Secrets)

Separate from signing, the daemon has an X25519 keypair at
`<system>/.ai/node/vault/` used for sealing/unsealing secrets.
This key is independent from the Ed25519 node identity — rotating
the node key does not affect sealed secrets.

## Signing as Compilation

The signing pipeline gives you properties analogous to a traditional
compiler, plus several that a compiler normally cannot provide.

### What a compiler gives you

| Property | Compiler | Signing Pipeline |
|---|---|---|
| **Integrity** | Compiled binary matches source | Content hash verified at every load |
| **Provenance** | (Usually absent) | Ed25519 signature identifies the author |
| **Trust gating** | (Link-time, implicit) | Trust store checked — untrusted keys rejected |
| **Structural validation** | Type checking | `deny_unknown_fields` YAML deserialization |
| **Cross-reference validation** | Missing symbol errors | Boot validator walks the registry for dangling refs |
| **Immutability** | Binary is frozen | Content-addressed hash locks the content |

### What makes it stronger than traditional compilation

1. **It runs every time** — A compiler runs once. The signing pipeline
   verifies every item at every daemon startup and at every request
   resolution. There is no stale compiled binary.

2. **It applies to configuration, not just code** — Routes, verbs,
   aliases, kind schemas, parsers, handlers, trust pins, and service
   endpoints are all verified. A misconfigured route can't sneak in any
   more than a syntax error can.

3. **It spans the trust boundary** — A compiler trusts its own standard
   library. The signing pipeline explicitly does NOT trust bundles —
   they are verified against the operator's trust store. The "standard
   library" is subject to the same verification as any extension.

4. **It composes across trust levels** — The weakest-link trust fold
   across extends chains means an untrusted ancestor taints the entire
   chain. A bundle-signed binary reached through a project-tier descriptor
   is capped at TrustedProject. This is dependency trust transitivity,
   computed dynamically per item.

5. **It is incremental by design** — Content addressing means only
   changed files need re-verification. But every load is still fully
   verified — there is no caching of trust decisions.

### What it is NOT

The signing pipeline is not static analysis in the program verification
sense. It cannot tell you "this tool will always terminate" or "this
directive will never call an API you don't have scope for." Runtime
capability enforcement handles behavioral correctness. The signing
pipeline is about integrity, provenance, and structural validity.
