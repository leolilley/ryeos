```yaml
id: decentralized-rye
title: "Decentralized Rye OS"
description: Self-certifying object system — identity from keys, state from signed CAS objects, compute from any node, confidentiality from sealed envelopes
category: future
tags: [decentralized, encryption, identity, cas, p2p, zero-knowledge, compute]
version: "0.5.0"
status: exploratory
```

# Decentralized Rye OS

> **Status:** Exploratory — architecturally grounded in existing primitives, not scheduled for implementation.

## The Vision

> Rye OS's decentralized architecture is built from self-certifying primitives, not centralized records: principals are keys, durable state is immutable CAS objects, mutable state is signed ref updates, authorization is signed delegation, confidentiality is sealed envelopes, and replication is peer sync. Centralized infrastructure becomes optional caches and bootstrap nodes, not roots of truth.

Rye already has the core primitives: Ed25519 signing, content-addressed storage, three-way merge, trust store, remote execution with fold-back. The decentralized vision is not "invent a new distributed system" — it's **promote the existing data model into the control plane.**

---

## Fundamental Primitives

Six primitives. Everything else composes from them.

| Primitive          | What it is                                          | What it powers                                     |
| ------------------ | --------------------------------------------------- | -------------------------------------------------- |
| **Principal**      | Ed25519 signing key + bound X25519 encryption key   | users, agents, nodes, orgs                         |
| **Signed Object**  | Canonical payload in CAS, signed by a principal     | directives, tools, knowledge, policies, results    |
| **Sealed Object**  | Signed plaintext encrypted for a group or recipient | encrypted intelligence, secrets, private metadata  |
| **Ref Update**     | Signed append-only pointer change (`prev → target`) | mutable project heads, bindings, registry pointers |
| **Project Policy** | Ref-updated authorization with grants and ownership | project writers, node execution rights, ownership  |
| **Sync Feed**      | Exchange of head hashes + missing CAS objects       | registry, replication, multi-peer storage          |

### Composition

- **Project state** = snapshot objects + ref updates
- **Remote execution** = node advertisement + execution request + secret envelopes + execution result + ref update
- **Webhook binding** = binding object + secret envelope + endpoint lease + event receipt
- **Distributed registry** = signed metadata objects + namespace claims + sync feeds/indexers
- **Encrypted intelligence** = sealed objects + epoch-based group keys + encrypted vault index

---

## Identity

### Principals are keys

A principal is an Ed25519 signing keypair + a bound X25519 encryption key. Your fingerprint IS your identity. No database lookup needed.

**Important:** Ed25519 is for signing/authentication only. Encryption uses a **separate X25519 key** bound to the signing identity. Signing and encryption are distinct operations with distinct keys.

Both keys are bound by a signed identity document:

```json
{
  "kind": "identity/v1",
  "principal_id": "fp:4b987fd4e40303ac",
  "signing_key": "ed25519:<pubkey>",
  "box_key": "x25519:<pubkey>",
  "recovery_delegates": ["fp:trusted-peer-1", "fp:trusted-peer-2"],
  "services": [
    { "type": "sync", "url": "https://node-a.example/sync" },
    { "type": "execute", "url": "https://node-a.example/execute" }
  ],
  "created_at": "2026-03-16T00:00:00Z"
}
```

Signed by the Ed25519 key. Stored as a CAS object. Publishable to any peer.

The `recovery_delegates` field declares which principals may countersign a key succession in case of compromise. This must be declared upfront — not ad-hoc at recovery time.

### Authentication: signed requests

For node-to-node and agent-to-node calls, **per-request signatures** replace API keys:

```
string_to_sign =
  "ryeos-request-v1\n" +
  METHOD + "\n" +
  PATH + "\n" +
  SHA256(BODY) + "\n" +
  TIMESTAMP + "\n" +
  NONCE + "\n" +
  AUDIENCE_NODE_ID
```

Headers: `key_id`, `timestamp`, `nonce`, `signature`.

Receiver verifies: signature valid, timestamp fresh, nonce unused, key authorized for requested action.

No central token database. No API key table. The key IS the credential.

### Authorization: project policy as mutable ref

Authentication ("you control this key") is separate from authorization ("this key may do X"). Authorization lives in the **project policy** — a ref-updated, append-only chain. Not standalone floating objects.

```json
{
  "kind": "project-policy/v1",
  "project_id": "proj:campaign-kiwi",
  "prev": "obj:<previous_policy_hash>",
  "owners": ["fp:owner1", "fp:owner2"],
  "grants": {
    "g:user1-rw": {
      "subject": "fp:user1",
      "actions": ["read", "write-ref:project/main"],
      "expires_at": "2026-06-01T00:00:00Z"
    },
    "g:node1-exec": {
      "subject": "fp:node1",
      "actions": ["execute"],
      "expires_at": "2026-06-01T00:00:00Z"
    }
  },
  "merge_policy": "three-way",
  "created_at": "2026-03-16T00:00:00Z"
}
```

Signed by an owner. Tracked via ref update at `proj:campaign-kiwi/policy`. Verifiable by any peer that trusts the project owners.

- **Granting access** = publish new policy with updated `grants` list via ref update
- **Revoking access** = publish new policy with the grant removed. Same mechanism — no separate revocation objects
- **Ownership transfer** = new policy with updated `owners` list, signed by a current owner
- **Total owner loss** = same problem as key compromise. Owners should be plural, and recovery follows the same delegate pattern as `identity/v1`

### Trust model

Trust has two layers: **local identity pinning** and **shared authoritative state**. These serve different purposes and must not be conflated.

#### Layer 1: Peer records — local identity cache (`.ai/config/keys/peers/{fingerprint}.toml`)

An evolution of the existing trust store. Today, trusted keys live at `.ai/config/keys/trusted/{fingerprint}.toml` as signed TOML files with identity binding. Peer records extend this pattern — same location convention, same `# rye:signed:` format, same 3-tier resolution (project → user → system):

```toml
# rye:signed:2026-03-16T00:00:00Z:HASH:SIG:FP
fingerprint = "4b987fd4e40303ac"
owner = "alice"
display_name = "Alice"
verified_via = "qr-exchange"
trusted_at = "2026-03-16T00:00:00Z"

[public_key]
pem = """
-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEA...
-----END PUBLIC KEY-----
"""

[box_key]
pem = """
-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEA...
-----END PUBLIC KEY-----
"""
```

Signed by you. This is your local assertion: "I trust this key is Alice." Peer records are **identity pinning and UX only** — they are not authoritative for group membership or authorization.

A peer record may cache a summary of groups/grants for display purposes, but the source of truth for "Alice is in team-alpha" is the signed `group-epoch/v1` object, and the source of truth for "Alice may write to my project" is the `project-policy/v1` object (tracked via ref update).

The existing `keys/trusted/` directory remains for backward compatibility — peer records are a superset (trusted key + box key). Migration: any `.toml` in `keys/trusted/` that lacks a `[box_key]` section is a legacy trust-only record.

#### Layer 2: Authoritative shared state — signed CAS objects

All authorization and group membership is expressed as signed CAS objects — verifiable by any peer, not dependent on anyone's local files:

| Object              | Signed by           | Authoritative for                   | Mutable via       |
| ------------------- | ------------------- | ----------------------------------- | ----------------- |
| `project-policy/v1` | Project owner       | "Key X may do Y on this project"    | Ref update        |
| `group-epoch/v1`    | Group owner         | "These members hold this epoch key" | Append-only chain |
| `key-succession/v1` | Old key or delegate | "Old identity → new identity"       | Append-only chain |
| `identity/v1`       | Principal           | "This key is me, here's my box key" | Key succession    |

Combined: "I trust fingerprint A belongs to Alice (peer record, local). The group-epoch object proves Alice holds team-alpha's current key (signed by group owner). The project policy proves Alice may execute on my project (signed by me, tracked via ref update)."

#### Groups (`.ai/config/groups/{group_id}/`)

Groups manage shared encryption keys. The **authoritative state** is the signed `group-epoch/v1` CAS object — it defines membership, binds epoch key blobs by hash, and forms an append-only chain.

Local filesystem layout:

```
.ai/config/groups/team-alpha/
├── group.toml              ← signed, local metadata (display name, owner)
├── epoch-3.key.enc         ← encrypted blob (unsigned — hash-bound from group-epoch object)
└── epoch-2.key.enc         ← previous epoch (read-only, for historical items)
```

`group.toml` — local signed metadata:

```toml
# rye:signed:2026-03-16T00:00:00Z:HASH:SIG:FP
group_id = "grp:team-alpha"
display_name = "Team Alpha"
created_by = "fp:owner"
created_at = "2026-03-16T00:00:00Z"
```

The authoritative group state is the `group-epoch/v1` CAS object:

```json
{
  "kind": "group-epoch/v1",
  "group_id": "grp:team-alpha",
  "prev": "obj:<previous_group_epoch_hash>",
  "state_seq": 5,
  "key_epoch": 3,
  "owners": ["fp:owner1", "fp:owner2"],
  "members": ["fp:4b987fd4e40303ac", "fp:9c2e7f31a8b04d12"],
  "wrapped_keys": {
    "fp:4b987fd4e40303ac": "obj:<wrapped_key_blob_hash>",
    "fp:9c2e7f31a8b04d12": "obj:<wrapped_key_blob_hash>"
  },
  "created_by": "fp:owner",
  "created_at": "2026-03-16T00:00:00Z"
}
```

Signed by a group owner. The `wrapped_keys` map binds each encrypted key blob by its CAS hash — unsigned blobs are authenticated because a signed object names their hashes. The `prev` field creates an append-only chain, preventing replay of old group state.

Peers discover the current epoch via a group ref: `grp:team-alpha/head`. This ref is updated (same ref-update mechanism as project refs) whenever a new `group-epoch/v1` is published. Peers track this head locally like any other ref.

**Bootstrap:** The group owner creates the first `group-epoch/v1` with `prev: null`, `state_seq: 1`, and `key_epoch: 1`. To wrap keys for members, the owner looks up each member's `identity/v1` document (a published CAS object) to get their X25519 box key. No out-of-band ceremony — identity documents are the bootstrap.

**Operations:**

- **Adding a member** = new `group-epoch/v1` with bumped `state_seq`, same `key_epoch`, updated `members` + `wrapped_keys`. The new member receives the current epoch key and can read all content encrypted under that key epoch — including items sealed before they joined. Access to content from earlier key epochs requires explicit old-epoch key sharing by the group owner
- **Revoking a member** = new `group-epoch/v1` with bumped `state_seq` AND bumped `key_epoch`, new group key, wrapped for remaining members. **No re-encryption of existing items.** O(members), not O(members × items)
- **Late joiners** do not receive old key epoch keys by default. Access to items encrypted under previous key epochs requires explicit old-epoch key sharing by the group owner — opt-in, not automatic
- **Ownership transfer** = `owners` field (plural) in `group-epoch/v1`. Any owner can sign new epochs. Same pattern as project policy — plural owners prevent single-point-of-failure
- **Multi-group items** = compose from primitives. An item readable by multiple groups gets multiple sealed objects (one `sealed-item/v1` per group), each encrypting the same plaintext independently. The resolver tries each group the reader belongs to. Plaintext is never stored as a publicly fetchable CAS object — each sealed object contains an independent ciphertext

#### Key succession

When a principal's key is rotated or compromised, a signed succession object links old identity to new:

```json
{
  "kind": "key-succession/v1",
  "prev_principal": "fp:old_key",
  "new_principal": "fp:new_key",
  "new_signing_key": "ed25519:<new_pubkey>",
  "new_box_key": "x25519:<new_pubkey>",
  "reason": "rotation",
  "timestamp": "2026-03-16T00:00:00Z",
  "proof": {
    "mode": "self-rotation",
    "old_key_signature": "<sig_by_old_key_over_canonical_body>",
    "new_key_signature": "<sig_by_new_key_over_canonical_body>"
  }
}
```

Two modes:

- **Routine rotation** — `proof.mode: "self-rotation"`. Signed by both the old key and the new key (proof of possession). Peers who trust the old key auto-update their peer record.
- **Compromise recovery** — `proof.mode: "delegate-recovery"`. Countersigned by a recovery delegate declared in the original `identity/v1` document, plus a proof-of-possession signature from the new key. Only predeclared delegates may countersign — this is not ad-hoc.

The succession chain is stored as CAS objects — identity is traceable through rotations.

**Box-key-only compromise** (signing key intact) triggers a group epoch bump for all groups the principal belongs to, plus a new `identity/v1` with a fresh box key. No key succession needed — the signing identity stays the same.

---

## Content-Addressed Storage

### Everything is a CAS object

All durable state is immutable, content-addressed, and signed:

- Directives, tools, knowledge → CAS objects (already exists)
- Project snapshots, manifests → CAS objects (already exists)
- Execution snapshots, runtime outputs → CAS objects (already exists)
- Ref updates → CAS objects (new)
- Project policies (with embedded grants) → CAS objects (new)
- Group epoch objects → CAS objects (new)
- Webhook bindings → CAS objects (new)
- Identity documents → CAS objects (new)
- Key succession records → CAS objects (new)

CAS objects are self-verifying — the hash IS the address. Any storage backend works: local filesystem, S3, IPFS, peer's disk. No trust required in the storage layer.

### Items vs files in manifests

The existing `SourceManifest` has two maps reflecting the current sync model:

| Map     | Covers           | Storage                                         | Sync policy                                |
| ------- | ---------------- | ----------------------------------------------- | ------------------------------------------ |
| `items` | `.ai/` paths     | `ItemSource` objects (signed, metadata-wrapped) | Always included                            |
| `files` | Non-`.ai/` paths | Raw content blob hashes                         | Opt-in via `sync.include` in `remote.yaml` |

By default only `.ai/` is synced (`sync.include: [.ai/]`). Projects opt non-`.ai/` files into sync by adding patterns to `sync.include`. Hard excludes (`.env`, `private_key.pem`, `*.secrets`) are enforced as a floor via `manifest.yaml` — projects can add exclusions but never remove system defaults.

This split matters for encrypted intelligence:

- **`.ai/` items** go through `ItemSource` wrapping — they become `sealed-item/v1` CAS objects when encrypted (sign plaintext, then encrypt the signed object)
- **Non-`.ai/` files** are raw blobs — they become `sealed-blob/v1` CAS objects when encrypted (encrypt content, manifest entry points to the sealed object)
- **Integrity** for non-`.ai/` files comes from the signed `ProjectSnapshot`, which authenticates all manifest entries transitively (the Git commit model)
- **Exclusion policy carries forward** — `manifest.yaml` hard excludes and `remote.yaml` sync patterns apply identically regardless of whether files are sealed or cleartext

### Sealed object types

Two CAS object types for encrypted content, keeping crypto metadata in the object rather than only in the manifest:

**`sealed-item/v1`** — for `.ai/` items (sign plaintext, then encrypt):

```json
{
  "kind": "sealed-item/v1",
  "item_id": "tools/my-secret-tool",
  "group_id": "grp:team-alpha",
  "group_epoch_hash": "obj:<group_epoch_object_hash>",
  "key_epoch": 3,
  "alg": "XChaCha20-Poly1305",
  "nonce": "<192-bit-csprng-hex>",
  "ciphertext_hash": "obj:<encrypted_blob_hash>"
}
```

**`sealed-blob/v1`** — for non-`.ai/` files:

```json
{
  "kind": "sealed-blob/v1",
  "path": "src/agent/core.py",
  "group_id": "grp:team-alpha",
  "group_epoch_hash": "obj:<group_epoch_object_hash>",
  "key_epoch": 3,
  "alg": "XChaCha20-Poly1305",
  "nonce": "<192-bit-csprng-hex>",
  "ciphertext_hash": "obj:<encrypted_blob_hash>"
}
```

The manifest stays simple: `path → sealed_* object hash`. AAD for each AEAD operation is derived canonically as `JCS({kind, item_id|path, group_id, group_epoch_hash})` — binding context to ciphertext and preventing substitution and confused-deputy attacks. AAD is not stored as a separate field; it is recomputed from the object's own fields during decryption.

### Sealed projects: the full picture

With sealing on both maps, the entire project — `.ai/` items and non-`.ai/` source code — becomes an encrypted, syncable, executable unit:

```
project_path/
├── .ai/                    ← items map: sealed-item/v1 objects
│   ├── directives/
│   ├── tools/
│   ├── config/
│   └── vault/
├── src/                    ← files map: sealed-blob/v1 objects
├── lib/
└── data/
```

The `SourceManifest` captures everything. The `ProjectSnapshot` signs the manifest. The group epoch key encrypts all sealed entries. A remote node receives the snapshot via sync, decrypts with its wrapped epoch key, materializes the full filesystem, and executes — same flow as today, just encrypted in transit and at rest.

`project_path` isn't just an identifier for `.ai/` items — it's the boundary of a complete sealed workspace. The existing sync include/exclude policy controls what's in scope, the manifest tracks integrity, and sealing adds confidentiality on top. No new sync protocol needed — the same `has/put` exchange works with sealed objects exactly as it does with cleartext ones.

### Sync protocol

Peer sync is simple HTTPS:

1. Exchange head summaries (ref sets)
2. Request missing CAS objects by hash
3. Verify locally (hash matches, signature valid, author trusted)
4. Update local head cache

This is the same protocol the current push/pull uses. It generalizes to any peer, not just a Modal volume.

---

## Mutable State: Signed Ref Updates

CAS is immutable. Mutability is expressed as **signed append-only ref update logs**.

A ref update is a new CAS object pointing from the previous head to a new target:

```json
{
  "kind": "ref-update/v1",
  "project_id": "proj:campaign-kiwi",
  "ref": "project/main",
  "prev": "obj:<old_ref_update_hash>",
  "target": "obj:<new_snapshot_hash>",
  "author": "fp:4b987fd4e40303ac",
  "timestamp": "2026-03-16T00:00:00Z"
}
```

Signed by the author. Peers keep a local head map (which ref update is current for each ref).

### Conflict resolution

The existing model works:

- If `prev` matches local head → fast-forward
- If two valid updates race → competing heads (fork)
- Resolve with three-way merge → new snapshot → merge ref update

This is Git's model. No consensus protocol needed. No CRDTs. The three-way merge and fold-back pattern already exists in the current codebase for `SourceManifest` path→hash maps (`rye/cas/merge.py`, `server.py::_fold_back`). Policy and group state would require a new domain-specific merge implementation using the same OCC pattern.

### Project policy

See [Authorization: project policy as mutable ref](#authorization-project-policy-as-mutable-ref). The policy is tracked at `proj:<id>/policy` and peers verify ref updates against it before accepting them.

### Concurrent updates to policy and group state

Content refs use three-way merge for conflicts. Policy and group state need domain-specific rules because authorization state isn't mergeable the same way source code is. Grants are keyed by stable `grant_id`, enabling base-aware 3-way diff: compare each side against the base to distinguish "removed" from "never present."

The distinction is between two kinds of operations:

**Additive operations** (add a grant, add a member within the same key epoch) — naturally mergeable via monotonic union. Two owners concurrently granting different users → union both grants. Both intents are preserved. Always safe.

**Destructive operations** (revoke a grant, bump key epoch / revoke a member, change owners) — inherently non-mergeable. A revocation produces a new epoch key (a new secret), and you can't union two different symmetric keys. These linearize via the `prev` chain.

The rule:

| Operation type                                          | Concurrent behavior                                                    | Mechanism                               |
| ------------------------------------------------------- | ---------------------------------------------------------------------- | --------------------------------------- |
| Two owners both add grants                              | Merge: union of grants (by `grant_id`)                                 | Base-aware 3-way diff on policy ref     |
| Two owners both add members (same key epoch)            | Merge: union of wrapped_keys                                           | Monotonic union on group state          |
| One owner adds grant, another revokes a different grant | Merge: addition + removal both apply (visible via base diff)           | Base-aware 3-way diff on policy ref     |
| Two owners both revoke different members                | Linearize: one `prev` matches head and wins, other retries on new head | Optimistic concurrency via `prev` chain |
| Two owners both bump key epoch                          | Linearize: same as above. Can't merge two epoch keys                   | Optimistic concurrency via `prev` chain |
| Owner set changes                                       | Linearize: always serialized. Too high-impact for automatic merge      | Optimistic concurrency via `prev` chain |

This is optimistic concurrency control — the same compare-and-swap the `prev` field already provides. Not a new primitive. The merge strategy for additive operations (base-aware union with stable IDs) is a straightforward CRDT-like rule; destructive operations and owner changes always linearize.

### Three-tier spaces

The existing precedence model maps to namespaces:

- `project/<project_id>/...` — project-scoped items
- `user/<principal_id>/...` — user-scoped items
- `system/<publisher_id>/...` — system-scoped items

Resolution order stays: **project → user → system**.

---

## Encrypted Intelligence

### Envelope encryption with epoch-based group keys

Encryption uses two distinct layers:

- **Key wrapping** — HPKE (X25519 + HKDF-SHA256 + AES-256-GCM) wraps the per-epoch symmetric group key for each member's X25519 public key. This is true HPKE with an encapsulated key (`enc`) output.
- **Content encryption** — XChaCha20-Poly1305 with the per-epoch symmetric group key. This is standard AEAD, not HPKE. XChaCha20's 192-bit nonces eliminate collision risk in multi-writer decentralized environments.

A single symmetric group key per epoch encrypts all items in a group. That key is wrapped for each member using HPKE.

The `group-epoch/v1` object (see Trust Model) is the authoritative record of who holds each epoch key and binds all wrapped key blobs by hash.

### Epoch-based key rotation

Each group maintains an append-only chain of `group-epoch/v1` objects. The `state_seq` increments on every change (member add, member remove, rekey). The `key_epoch` increments only when the symmetric group key changes (member removal or explicit rekey):

| Key Epoch | Key state   | Who has it               | Used for                       |
| --------- | ----------- | ------------------------ | ------------------------------ |
| 1         | Archived    | All members from epoch 1 | Reading historical items       |
| 2         | Archived    | All members from epoch 2 | Reading items from that period |
| 3         | **Current** | Current members only     | Encrypting new items + reading |

New items always use the current key epoch. Old items remain readable with their epoch key. Revocation is O(members) — wrap one new key — not O(members × items).

State changes use the same append-only model as ref updates — each `group-epoch/v1` has a `prev` field pointing to the previous state object. Peers discover the current group state via the signed group ref `grp:team-alpha/head`, updated via the standard ref-update mechanism whenever a new `group-epoch/v1` is published. Authority comes from the signed ref, not from comparing sequence numbers.

### Nonce policy

XChaCha20-Poly1305 with 192-bit nonces is the default AEAD for sealed items and blobs. With CSPRNG-generated nonces, collision probability is negligible even across many distributed writers sharing one epoch key — no per-epoch usage limits or nonce coordination required.

AES-256-GCM may be used where FIPS compliance is required. In that case: 96-bit CSPRNG nonces, rotate the epoch key before 2^32 encryptions per writer, and consider deriving per-item subkeys from the epoch key via HKDF to isolate nonce spaces across writers.

### Seal format (on-disk) vs sealed objects (CAS)

Two representations, same relationship as `# rye:signed:` (on-disk) vs `ItemSource` (CAS):

- **`rye:sealed:...`** — on-disk format for `.ai/` items (directives, tools, knowledge). The file on disk has an encrypted body with a seal line, just as today it has a plaintext body with a signature line
- **`sealed-item/v1`** / **`sealed-blob/v1`** — CAS representation after ingestion. The manifest points to these objects. This is what sync, materialization, and verification operate on

For `.ai/` items, sealing is a two-step process — **sign-then-encrypt**:

1. The plaintext is signed with the existing `rye:signed:` format (inner signature, authenticates authorship)
2. The signed plaintext is encrypted, producing a seal line:

```
rye:sealed:TIMESTAMP:OUTER_HASH:OUTER_SIG:FP:GROUP_ID:EPOCH_HASH:NONCE
```

| Field        | Purpose                                                                              |
| ------------ | ------------------------------------------------------------------------------------ |
| `OUTER_HASH` | SHA256 of the **ciphertext** (not plaintext)                                         |
| `OUTER_SIG`  | Ed25519 signature of the ciphertext hash (allows integrity check without decryption) |
| `FP`         | Fingerprint of the sealer (may differ from the inner plaintext signer)               |
| `GROUP_ID`   | Which group's key encrypts this item                                                 |
| `EPOCH_HASH` | CAS hash of the `group-epoch/v1` object (not an integer)                             |
| `NONCE`      | XChaCha20 nonce (192-bit CSPRNG, hex-encoded)                                        |

The inner `rye:signed:` line (embedded in the plaintext before encryption) authenticates the author and content. The outer seal signature authenticates the ciphertext — you can verify an item is authentic and untampered without being able to read it. Two signatures, two purposes.

For protocol objects and non-`.ai/` files, crypto metadata lives in the `sealed-item/v1` or `sealed-blob/v1` CAS objects (see Sealed Object Types above) rather than inline.

### Vault mode (metadata-hiding)

The `.ai/` directory becomes a flat vault — no readable filenames, no category folders:

```
.ai/
├── vault/
│   ├── 7a3f.enc
│   ├── b91c.enc
│   └── e204.enc
└── vault.idx.enc         ← encrypted index: maps item IDs → vault files
```

The vault index is encrypted with the current group epoch key. Resolution: decrypt index → lookup item ID → select epoch key from seal → decrypt item → verify inner signature → run.

**Privacy note:** Vault mode hides filenames and content, but seal metadata (signer fingerprint, group_id, epoch_hash) remains visible on the outer seal line. This is sufficient for most use cases. Full metadata privacy (hiding who sealed what and for which group) requires moving seal metadata inside the ciphertext — an advanced path.

Start with a single encrypted index file per vault/group. CAS already provides immutability and peer-based recovery — if the index is corrupted, fetch it from any peer that has the CAS object. Sharding or Merkle-tree indexing is an optimization for later, only needed when vault indexes exceed tens of MB or partial sync becomes painful.

### Mixed mode

Not everything needs to be vaulted. Cleartext items and vault items coexist:

```
.ai/
├── directives/                    ← cleartext items
│   └── onboarding.md
├── vault/                         ← encrypted items
│   ├── 7a3f.enc
│   └── b91c.enc
├── vault.idx.enc
└── config/
    ├── keys/
    │   ├── signing/               ← your signing keypair (existing)
    │   ├── trusted/               ← legacy trust-only keys (existing)
    │   └── peers/                 ← peer records (new, superset of trusted/)
    └── groups/                    ← group key material (new)
        └── team-alpha/
```

Resolver checks standard directory first (existing behavior), then vault index. Additive — existing items and workflows don't break.

### Sharing scopes

| Scope    | Key                        | What it covers                     |
| -------- | -------------------------- | ---------------------------------- |
| Personal | (your own key)             | Private tools, personal knowledge  |
| Team     | `grp:team-alpha` epoch key | Shared workflows, domain knowledge |
| Org      | `grp:org-acme` epoch key   | Organization-wide directives       |
| Public   | (no encryption)            | Open-source items                  |

### What the agent experiences

Nothing changes. The three MCP tools work identically:

| Tool      | Without encryption        | With encryption                              |
| --------- | ------------------------- | -------------------------------------------- |
| `fetch`   | Read item content / search metadata | Decrypt + read / search decrypted index |
| `execute` | Verify → run              | Verify → decrypt → run                       |
| `sign`    | Sign item                 | Seal item (sign + encrypt)                   |

---

## The Agent is the Substrate

### Why there is no agent identity problem

Many decentralized systems struggle with agent identity: if Agent A spawns Agent B on another node, what key does B use? How do you build delegation chains? How do you scope authority across a graph of autonomous agents?

Rye OS sidesteps this entirely. **There is one agent. It is the substrate.** It operates across many threads, on many nodes, triggered by many events — but it is always the same agent executing within bounds set by its owner.

This is not a philosophical nicety — it eliminates an entire class of cryptographic and authorization complexity:

| Multi-agent model (others)                        | Single-substrate model (Rye OS)                          |
| ------------------------------------------------- | -------------------------------------------------------- |
| Each agent needs its own keypair                  | The user and nodes are the only principals               |
| Delegation chains: user → agent A → agent B → ... | No delegation chains. Scope comes from bindings + policy |
| Agent identity revocation                         | Node revocation (already covered by project policy)      |
| Agent-to-agent trust negotiation                  | No negotiation. The agent trusts itself across threads   |
| Confused deputy across agent boundaries           | One agent, one authority context per execution           |
| Secret forwarding between agents                  | Secrets are encrypted to nodes, not agents               |

### How authorization flows without agent identity

The three existing objects already cover every execution scenario:

**1. Interactive execution** (user at the terminal):

```
User signs execution request → Node executes → Node signs result → User accepts via ref update
```

The agent runs on the node with the user's authority, scoped by the execution request. No agent key needed — the user's request signature IS the authorization.

**2. Autonomous execution** (webhook-triggered, no user in the loop):

```
Event arrives → Node checks webhook binding → Node verifies binding owner's signature →
Node executes scoped directive → Node signs result → Ref update published
```

The webhook binding (signed by the owner) defines: which directive, which project ref, what auth. The project policy (signed by the owner) defines: this node may execute. The agent running the directive is the owner's intelligence — it doesn't need its own identity because the binding IS the pre-authorized scope.

**3. Multi-node execution** (agent thread spawns work on another node):

```
Node A executing directive → needs GPU compute → pushes to Node B →
Node B executes within project policy bounds → signs result → syncs back to Node A
```

Node A doesn't "delegate" to Node B. The project policy already grants both nodes execute rights. Node B verifies the execution request against the project policy, same as any other request. The agent on Node B is the same agent — different thread, different hardware, same authority context.

### What this means for secrets

Secrets are encrypted to **nodes**, not agents. When a webhook-triggered execution needs an API key:

1. The webhook binding references a logical secret ID, and the endpoint lease holds a sealed envelope for the hosting node
2. The secret is encrypted to the executing node's X25519 box key
3. The node decrypts it at execution time

The agent never "holds" a secret — the node holds it for the duration of execution. When execution ends, the node's runtime is torn down. The secret envelope is bound to a specific node, not to an agent session that might persist or migrate.

### When you actually need agent subkeys (advanced path)

Delegated subkeys become relevant only when:

- **Defense-in-depth**: you want cryptographic enforcement that a webhook-triggered execution can't exceed its declared scope, even if the node is compromised. A scoped subkey with limited actions and TTL provides this
- **Offline owner**: the owner is unreachable for extended periods and the agent needs to rotate secrets or update refs autonomously. A pre-signed delegation with a scoped ephemeral key handles this
- **Audit attribution**: you need to distinguish "user did this interactively" from "the agent did this autonomously via webhook" in the signature trail. Different signing keys make this cryptographically verifiable

These are real scenarios but not prerequisites. The base model — user key + node key + binding scope — handles the common case.

---

## Decentralized Compute

### Current model: named remotes

Today, nodes are configured as named entries in `remote.yaml`:

```yaml
remotes:
  default:
    url: "https://ryeos-remote--execute.modal.run"
    key_env: "RYE_REMOTE_API_KEY"
  gpu:
    url: "https://gpu-worker--execute.modal.run"
    key_env: "GPU_REMOTE_API_KEY"
```

Capability routing is by name — the user calls `thread="remote:gpu"` or a graph node specifies `"remote": "gpu"`. There's no capability query or advertisement protocol. Auth is Bearer token (API key or JWT via Supabase). Node identity uses TOFU — the client pins the remote's Ed25519 public key on first contact via `/public-key`. Secrets are stored server-side in Supabase, injected as environment variables at execution time.

This works. The decentralized model promotes it, not replaces it.

### Future model: self-describing nodes

A named remote becomes a **node advertisement** — a signed CAS object that makes the remote.yaml entry self-certifying:

```json
{
  "kind": "node-advertisement/v1",
  "node_id": "fp:node123",
  "signing_key": "ed25519:<pubkey>",
  "box_key": "x25519:<pubkey>",
  "endpoint": "https://node.example",
  "runtimes": ["python", "node", "bash"],
  "resources": {
    "gpu": true,
    "gpu_type": "A100",
    "max_concurrent": 4
  },
  "sync_protocol": "ryeos-cas/v1",
  "created_at": "2026-03-16T00:00:00Z"
}
```

Signed by the node's key.

**Node resources vs thread capabilities — these are separate concerns:**

| Concept                 | Question it answers                     | Where it lives                                          | Enforced by                            |
| ----------------------- | --------------------------------------- | ------------------------------------------------------- | -------------------------------------- |
| **Thread capabilities** | "Is this thread _allowed_ to run bash?" | Directive `<permissions>` block, `capability_risk.yaml` | SafetyHarness (fail-closed, `fnmatch`) |
| **Node resources**      | "Does this node _have_ a GPU?"          | `node-advertisement/v1` (`runtimes`, `resources`)       | Execution routing / scheduler          |

Capabilities are about **authorization** — they attenuate down the thread hierarchy and are enforced identically on local and remote nodes (the materialized `.ai/` on a remote node runs through the same SafetyHarness). Node resources are about **scheduling** — matching a workload's requirements to a node that can fulfill them.

A directive that declares `<execute><tool>rye.bash.*</tool></execute>` with `<acknowledge>elevated</acknowledge>` is _authorized_ to run bash. Whether it runs on `default` or `gpu` depends on the node's resources, not its capabilities. Both nodes enforce the same capability checks.

The current model handles this correctly: capability routing is by name (`thread="remote:gpu"`), and the remote node materializes the full `.ai/` environment including `capability_risk.yaml`. The future model makes resource matching structured (query by `runtimes` and `resources` fields) instead of by name, but capability enforcement stays unchanged.

What changes from the current model:

| Current (remote.yaml)                 | Future (node-advertisement/v1)                                          |
| ------------------------------------- | ----------------------------------------------------------------------- |
| URL + key_env in config file          | Signed CAS object, discoverable via sync                                |
| TOFU key pinning via `/public-key`    | Key in the advertisement, pinned in peer record                         |
| Capability by name ("gpu")            | Structured capabilities (`runtimes`, `resources`)                       |
| Bearer token auth (API key / JWT)     | Per-request signatures (see Authentication)                             |
| Secrets stored server-side (Supabase) | Secrets as sealed envelopes (HPKE to node's box key, with `enc` output) |
| One user per remote (user_id scoping) | Multi-user via project policy grants                                    |

The current `remote.yaml` becomes a local cache of node advertisements — the same pattern as peer records caching identity documents. The source of truth is the signed advertisement, not the config file.

### Execution flow

The flow stays the same — it's already the right shape:

| Step       | Current implementation                   | Future protocol                                  |
| ---------- | ---------------------------------------- | ------------------------------------------------ |
| 1. Sync    | `POST /objects/has` + `/objects/put`     | Same `has/put` protocol, any peer                |
| 2. Request | `POST /execute` with Bearer token        | Signed execution request (per-request signature) |
| 3. Execute | Materialize temp `.ai/`, run executor    | Same materialization, same executor              |
| 4. Results | Return snapshot hash + new object hashes | Same, but node signs result as CAS object        |
| 5. Pull    | `POST /objects/get`                      | Same sync protocol in reverse                    |

The execution protocol is already correct — push, execute, fold-back. The decentralized model replaces the auth mechanism (Bearer → signatures) and coordination store (Supabase → signed CAS objects), but the sync protocol and materializer are unchanged.

### Secrets for execution

**Current:** Secrets are stored server-side in Supabase, scoped to the authenticated user. They never appear in CAS — injected as environment variables at execution time only.

**Future:** Secrets are encrypted to the target node's X25519 box key using HPKE, bound to a specific execution request:

```json
{
  "kind": "secret-envelope/v1",
  "aud": "fp:node123",
  "exec_request": "obj:<request_hash>",
  "name": "OPENAI_API_KEY",
  "alg": "HPKE-Base-X25519-SHA256-AES128GCM",
  "enc": "<hpke_encapsulated_key_hex>",
  "ciphertext": "<hpke_ciphertext_hex>"
}
```

AAD for the HPKE seal operation is derived canonically as `JCS({kind, aud, exec_request, name})`, binding the secret to its intended audience and execution request. The `enc` field is the HPKE KEM output (encapsulated key) — the recipient uses their X25519 private key to decap and derive the AEAD key. Nonce management is internal to HPKE's AEAD context, not a user-visible field.

**Security boundary:** Encrypting to a node protects secrets in transit and at rest before execution. Once the node executes code with the secret, the node operator can see it. True protection from untrusted executors requires TEE attestation — an advanced path, not a prerequisite. The envelope carries no `expires_at` — expiry is unenforceable after decryption, so the protocol doesn't pretend otherwise. Rotation and short-lived credentials at the secret source (e.g., short-lived API tokens) are the real mitigation.

**Migration:** During Phase 1–2, both models coexist. Server-side secrets (Supabase) remain for backward compatibility while sealed envelopes are added. Sealed envelopes take precedence when both are available.

### Webhook bindings as signed objects

A webhook binding is a signed policy object:

```json
{
  "kind": "webhook-binding/v1",
  "binding_id": "wh:abc123",
  "owner": "fp:owner",
  "target": {
    "item_type": "directive",
    "item_id": "campaign-kiwi/agent/email/handle_inbound",
    "project_ref": "project/main"
  },
  "auth": {
    "type": "hmac-sha256",
    "secret_id": "sec:wh-abc123-hmac"
  },
  "enabled": true
}
```

Signed by the owner. Any node that trusts the owner can verify and honor the binding. The binding references a logical `secret_id`, not a specific sealed envelope — the node-specific wrapped secret lives in the endpoint lease, allowing re-wrapping when the webhook migrates between nodes.

An **endpoint lease** records which node hosts the webhook ingress and holds the node-wrapped secret:

```json
{
  "kind": "webhook-endpoint-lease/v1",
  "binding_id": "wh:abc123",
  "host_node": "fp:node123",
  "public_url": "https://node.example/execute",
  "secret_envelope": "obj:<sealed_secret_hash_for_node123>",
  "expires_at": "2026-06-01T00:00:00Z"
}
```

Signed by the hosting node. When the lease migrates to a different node, the binding owner wraps the secret for the new node's box key and issues a new lease.

---

## Distributed Registry

### Registry = indexer, not root of truth

The registry is not a naming authority — it's a convenience layer:

1. **CAS object transport** — any peer can provide
2. **Discovery/search** — optional indexers/mirrors
3. **Verification** — always local via hashes and signatures

The current registry becomes the first indexer, cache, and namespace convenience service — but not the source of truth.

### Self-certifying names

Primary naming uses fingerprints:

- `fp:4b987fd4/tools/utilities/data-pipeline`
- `proj:abc123/refs/main`

Human-friendly names (`alice/data-pipeline`) are a social layer, not a cryptographic primitive. Namespace claims are signed objects trusted by peers/indexers:

```json
{
  "kind": "namespace-claim/v1",
  "namespace": "alice",
  "owner": "fp:4b987fd4",
  "created_at": "2026-03-16T00:00:00Z"
}
```

### Search trade-offs

Three properties — pick two:

- Private metadata
- Rich global search
- No central help

Practical modes:

- **Discoverable mode**: cleartext metadata, encrypted payload — global search works
- **Vault mode**: encrypted metadata, local-only or group-local search — maximum privacy

---

## Migration Path

### Principle

> Centralized services should become caches, mirrors, and bootstrap nodes before they are removed.

### Phase 0 — Current state (now)

- Supabase = auth + coordination DB
- Modal = execution provider
- CAS exists, signatures exist
- API keys for authentication

### Phase 1 — Add future identity model

Keep Supabase/Modal, but layer the future protocol on top:

- Signed identity documents with recovery delegates
- X25519 encryption keys per principal/node
- Request signing alongside API keys (dual auth)
- Node advertisements
- Peer records alongside existing trust store (migration shim)

Supabase remains a convenience store, but keys become the primary identity.

### Phase 2 — Control plane rows → signed objects

Represent as signed CAS objects:

- Project policy (with grants, tracked via ref update)
- Ref updates
- Group epoch objects (with membership, tracked via group ref)
- Webhook bindings
- Execution requests/results

Supabase becomes a **materialized view** — it indexes signed objects for query/UI, but is no longer the source of truth.

### Phase 3 — Modal as one node type

Wrap Modal with the same node protocol as self-hosted nodes:

- Advertise capabilities
- Accept signed execution requests
- Decrypt node-targeted secret envelopes (HPKE)
- Emit signed execution results

"Remote execution" becomes portable across providers.

### Phase 4 — Peer sync

Implement HTTPS sync between peers:

- Exchange head summaries / ref sets
- Request missing CAS objects
- Verify locally
- Update local head cache

Any node can store, sync, and execute.

### Phase 5 — Federate registry/index/search

Multiple mirrors/indexers. Current registry becomes one default peer among many.

### Phase 6 — Optional removal of central bootstrap

Only after the object protocol is stable. Reduce dependence on Supabase for discovery/bootstrap.

---

## Guardrails and Constraints

### Cryptographic construction

- **Ed25519** for signing only. **X25519** for encryption only. Both keys bound to one identity, but the protocol treats them as distinct operations.
- **HPKE (X25519 + HKDF-SHA256 + AES-256-GCM)** for key wrapping and node-targeted secret envelopes. Not a bespoke construction — use a standard HPKE implementation. HPKE is used only where there is a recipient public key.
- **XChaCha20-Poly1305** as the default AEAD for sealed items and blobs (content encryption under shared group epoch keys). 192-bit nonces eliminate collision risk across distributed writers. AES-256-GCM is available as a FIPS fallback with per-item subkey derivation.
- **AAD binding** on all AEAD operations. Authenticated additional data is derived canonically as `JCS({kind, item_id|path, group_id, group_epoch_hash})` from the sealed object's own fields. This prevents substitution and confused-deputy attacks.

### Canonical serialization

For protocol objects (ref updates, grants, group epochs, bindings), use canonical JSON (JCS) or DAG-CBOR. Do not sign ad-hoc text formatting for protocol objects.

### Authority boundaries

- **Peer records** are local identity cache — never authoritative for group membership or authorization
- **`project-policy/v1`** is authoritative for project authorization (grants, ownership). Tracked via ref update
- **`group-epoch/v1`** objects are authoritative for group membership and key distribution. Tracked via group ref
- **`identity/v1`** objects are authoritative for key binding and recovery delegates. Mutable via key succession

### Mutable local state is still needed

Even in a decentralized system, each node needs small mutable caches:

- Used auth nonces (replay protection)
- Webhook dedupe IDs
- Current head map (ref heads, group epoch heads)

"Database-less" does not mean "stateless."

### Revocation is epoch-based, not retroactive by default

Member revocation bumps the group epoch via a new `group-epoch/v1` object. New items are encrypted with the new epoch key. Old items remain readable with old epoch keys by anyone who held that key at the time. Retroactive re-encryption is available as an explicit, expensive operation for high-security scenarios — it is never automatic.

### Late joiners and historical access

New group members receive only the current epoch key. They cannot read items encrypted under previous epochs unless the group owner explicitly wraps old epoch keys for them. This is a policy decision, not a protocol limitation.

### Untrusted executors are still untrusted

Secret envelopes protect delivery, not execution misuse. The protocol does not pretend otherwise — envelopes carry no expiry fields. Stronger guarantees require TEE attestation or trusted node sets — an advanced path.

### Global naming is a social problem

Self-certifying names (`fp:abc/tool`) are easy. Human-friendly global names (`alice/tool`) require trust in a naming authority. Treat them as optional convenience.

### Key compromise recovery

Key succession requires countersignature from a recovery delegate **predeclared in the original `identity/v1` document**. Ad-hoc recovery delegates are not accepted — this prevents social engineering attacks where an attacker convinces an arbitrary peer to countersign a fraudulent succession.

Box-key-only compromise (signing key intact) does not require key succession — issue a new `identity/v1` with a fresh box key and bump affected group epochs.

---

## Advanced Path (not prerequisites)

Consider these only when triggered by specific needs:

| Trigger                                                  | Advanced feature                                    |
| -------------------------------------------------------- | --------------------------------------------------- |
| Many concurrent writers overwhelm three-way merge        | CRDTs                                               |
| Cross-org global naming without trusted indexers         | DHT/gossip discovery, threshold namespace ownership |
| Secrets must be safe from node operators                 | TEE attestation                                     |
| Metadata privacy requirements block cleartext discovery  | Encrypted searchable indexes                        |
| Agent delegation needs short-lived subkeys               | Delegated subkeys with TTLs                         |
| Retroactive revocation at scale                          | Forward-secret key ratchet (double ratchet variant) |
| Vault index exceeds tens of MB or partial sync is needed | Sharded Merkle-tree index                           |
| High group churn with many concurrent rekeys             | MLS-like group key schedule                         |
| Recovery must survive without predeclared delegates      | Threshold recovery / org-backed attestations        |

---

## Relationship to Current Work

The Steps 9–10 implementation (webhook resolve, inbound email wiring) builds toward this vision:

| Current decision                                               | How it leads here                                              |
| -------------------------------------------------------------- | -------------------------------------------------------------- |
| Provider-agnostic architecture (no proxy, engine handles auth) | Any node can be an execution node                              |
| `project_path` as abstract identifier                          | Not tied to any database — becomes a sealed workspace boundary |
| HMAC webhook auth with `resolve_execution()`                   | Future: signed binding objects, same verification flow         |
| User space separation from projects                            | Independent ref updates per space                              |
| CAS-native everything                                          | CAS objects can live anywhere, sync to any peer                |
| Ed25519 signing + trust store                                  | Foundation for principal-based identity + peer records         |
| `SourceManifest` items/files split                             | Natural sealing boundary: sealed-item + sealed-blob            |
| `manifest.yaml` hard excludes + `remote.yaml` sync policy      | Exclusion policy carries forward to sealed projects            |
