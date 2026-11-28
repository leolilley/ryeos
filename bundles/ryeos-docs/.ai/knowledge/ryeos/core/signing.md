<!-- ryeos:signed:2026-11-28T18:42:50Z:bcf4ae6261b6c778141b40e339ef544847ee18c44be64342f553a69873658875:Xq3uJbmsZdKseZfTPVnfxB8qdVx9l3mJJ1GlQQLoEpRLsMLO0p6IedRaI4eytEBk0rMnr5XlEthN71qP4qiGBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

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
| **Authoring key** | Items the runtime authors; machine drafts, untrusted until an operator endorsement covers them (see [Endorsements](#endorsements)) | `<system>/.ai/node/authoring/signing/private_key.pem` |
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
key signed it. An installed bundle's name is reserved the same way, so a
project item under another installed bundle's name is refused too. See
[Spaces](spaces.md).

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
| `scope` | `spaces` (bundle, project, node), `kinds` (item kinds, or all), `projects` (owner+name identities `pid:v1:<owner>/<name>` or `own:<name>`; empty is unscoped) |
| `status` | `active`, `retired`, `revoked` |

Defaults: a publisher is scoped to the bundle space for every kind; the
operator key is scoped to the project and node spaces for every kind; the
authoring key is scoped to the project space with **no kinds** (`kinds: []`
admits nothing), and that scope is the only one it may hold: its registry scope
never decides trust, so items it signs are untrusted until an operator endorses
them (see [Endorsements](#endorsements)).

`scope.projects` names projects by owner+name identity, never by path. An
entry is `pid:v1:<owner>/<name>` (a named owner principal) or `own:<name>` (the signer's own
key as owner). A project's name is the `name:` of its `.ai/manifest.yaml` (the same name its
items are namespaced under; renaming a project means changing that name and
registering the checkout again); the manifest's signature plays no part in
identity, and the owner never comes from the tree. A scoped signer is trusted only where the node has established that
identity:

- a live checkout the launcher registered under its own owner
  (`ryeos project register`), whose tree still declares that name, launched by a
  principal holding `ryeos.write.project.live`;
- a pushed head this node holds for that owner and name at exactly the run's
  snapshot.

The identity is sealed when a run is admitted and is never re-derived on
resume. Anywhere else (an unregistered checkout, another owner's project of the
same name, a pinned generation whose owner's head this node does not hold) the
run states no identity, and a scoped signer is untrusted there. A UI session in
that position reports "no project identity for this session".

A signer with an empty `projects` list is unscoped: it is trusted in every
project context, identified or not. Registration matters only for scoped
signers. `ryeos node doctor` lists unscoped project-space signers other than the
operator and the authoring key under `unscoped_project_signers`.

`ryeos trust scope <fp> --project pid:v1:<owner>/<name>` (or `own:<name>`) replaces
the list; `ryeos trust scope <fp> --any-project` clears it. Adding the project
space needs one of the two, so a scope never becomes all-projects by accident;
a scope that leaves the project space drops its projects. A signer record
written before owner+name identity that still lists project paths is refused
by name (`trust_record_schema_unsupported`): only that signer is unusable, the
node starts, and `ryeos node doctor` names it under `signer_records_refused`.
Re-scope it with an identity, or out of the project space.

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
the operator key (in the operator's own process) or, when the node reseals it,
the node key under the operator's signed request. Anything else is refused as
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

## Publication Pull Verification

A node pulling a published bundle from another node pins that node's key
(its fingerprint, and the signing key that hashes to it) and verifies three
signed documents: the publication record, the node's countersignature over it,
and the bundle manifest. It does not compare the other node's build: the
pinned key and the documents' own formats decide.

- The record and the manifest each name a format, compared exactly. One this
  build does not read is refused `publication_format_unsupported`, naming the
  document (the record or the manifest), the format it carries and the
  format this node reads.
- A countersignature's format is fixed by the domain it is signed under
  (`ryeos-publication-node-v1`). A countersignature of another format, like a
  forged one, does not verify, and is refused with the signature code; the
  refusal names the domain it was checked under.

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

## Endorsements

An endorsement is a signed statement, separate from the item, by which a pinned
operator key makes items the node's authoring key signed trusted. It leaves the
item untouched, so who authored it and the digest that was reviewed stay as they
were.

| Form | Names | Covers |
|---|---|---|
| per-change | the exact items: ref, kind and the digest of the signed bytes after the signature line | those bytes only; one changed byte returns the item to `untrusted_project` |
| standing | kinds within one project (`pid:v1:<owner>/<name>`), until a time at most 30 days after it is made | whatever the authoring key signs of those kinds where that identity is established, until it ends |

```bash
ryeos --project . endorse --item view:acme/home --item surface:acme/main
ryeos --project . endorse --standing --kind knowledge --lifetime-secs 604800
ryeos endorse list                # shows `in_force`, and `inert` with a code for one that covers nothing
ryeos endorse revoke --endorsement <digest> --reason "no longer wanted"
```

`ryeos endorse` signs the endorsement with the operator key inside the CLI; the
daemon never holds that key. The project is the checkout's declared name owned
by the endorsing operator, `pid:v1:<owner>/<name>` with the operator's principal as owner; the CLI prints it before
signing and refuses a checkout that declares no name, and the node refuses an
endorsement for a project not owned by the principal the endorser speaks for
(`endorsement_project_not_endorsers`). An endorsement covers items only
where that identity is established (a registration or a held head), the same
gate a scoped signer meets; `ryeos node doctor` lists endorsements that cover
nothing on the node under `endorsements_covering_nothing`. It reads the node's endorsement ledger head, signs
a body that carries the project, the endorser, the exact targets, the time, a
nonce and that head, and submits it to the running node. The node checks the
signature against its signer registry (the endorser must be an active
`operator`), countersigns the document with a receipt time and stores it under
`<state>/endorsements/`. Two endorsements signed against the same head cannot
both apply: the second is refused with `endorsement_ledger_moved`, sign again.
The body names its own time, and a time more than two minutes from the node's
clock is refused.

The engine's trust decision consults the endorsements: an authoring-key item in
project space is trusted (still clamped like every project item) only when a
live endorsement covers its exact digest, or a live standing endorsement covers
its kind in its project. Everything else from the authoring key is untrusted.
Endorsement is narrow-only: it chooses `trusted_project` over `untrusted_project`
and nothing else. It never applies to another role's key, cannot make an
unpinned or revoked key trusted, and cannot lift a revocation. A running node
follows a new or revoked endorsement without a restart.

A standing endorsement trusts everything the authoring key signs of a kind from
then on, including what an agent writes later. That is safe only for kinds whose
items never run or invoke anything, so each kind says whether it allows one in
its signed kind schema (`standing_endorsement: allowed`; the default is refused).
Today `knowledge`, `picture` and `view_set` allow it. The executing kinds
(directive, graph, tool, config, service, runtime and the rest), and `view` and
`surface`, whose actions and tiles invoke refs, do not: endorse those one change
at a time, by digest, after reviewing the change. Rules:

- `ryeos endorse --standing --kind <kind>` for a kind that does not allow it is
  refused with `endorsement_kind_not_standing`, before anything is signed.
- A standing endorsement must end: `--lifetime-secs` is required, at most 30 days
  (2592000 seconds), and the expiry is printed before signing
  (`endorsement_standing_lifetime_required`, `endorsement_standing_lifetime_too_long`).
- The node judges kinds by its own installed, verified kind registry, never by
  anything in a project. A stored standing endorsement whose kind does not allow
  one, or that has no bounded life, covers nothing (it fails closed) and is
  listed with `inert`; so does every standing endorsement until the node has
  paired its trust store with its registry. When you make a standing endorsement
  again after an upgrade, give it a lifetime and an allowed kind.

A launch carries the endorsements its request froze in its signer list: each
authoring-role signer's entry holds a snapshot of the live endorsements of the
launch's project. Every config loader the daemon builds for the launch
(execution limits, method runtime config) and the runtime (launch envelope
schema version 6) judge authoring-key items by that snapshot against the
launch's own project identity, its canonical root, not a checkout path. The
signer list's digest, which every config dependency proof and cache key records,
covers the snapshot, so a proof made under an endorsement stops matching when the
endorsement is revoked or replaced. A launch keeps the view it froze; the next
launch freezes the new one. One request builds every signer list from the single
trust view it froze at admission, so a revocation between its method-config load
and its limits load never gives two parts of it two trusts. Offline commands
read the node's recorded endorsements when they load the registry; the only
signer list built outside a launch is the one for a bundle release source
generation, which holds publisher-signed configs and so carries none. An envelope of another schema version is refused;
runtime binaries ship in bundles, so repopulate bundles after this change.

Revoking writes a node-signed tombstone named by the endorsement digest. The
items it covered are untrusted again, and the tombstone stays in force if the
endorsement file is put back. Every acceptance and revocation is a node-journal
record (`endorsement.accepted`, `endorsement.revoked`). An endorsement that
names a delegation is refused (`endorsement_delegation_unsupported`).

### A node pinned under the old authoring scope

`ryeos trust scope` no longer widens an authoring key: it refuses with
`trust_authoring_scope_retired`. A node whose authoring record was widened
earlier refuses to load its registry, with the same code, naming the key. Stop
the node and run `ryeos trust scope <authoring-fingerprint>` with no flags to
restore the authoring scope; items the key signed are then untrusted until
endorsed.
