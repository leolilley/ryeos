```yaml
id: encrypted-shared-intelligence
title: "Encrypted Shared Intelligence"
description: Encrypt the entire .ai/ intelligence layer with group keys — directives, tools, knowledge become a cryptographically-gated shared knowledge fabric
category: future
tags: [encryption, sharing, intelligence, security, group-keys, zero-knowledge]
version: "0.1.0"
status: exploratory
```

# Encrypted Shared Intelligence

> **Status:** Exploratory — architecturally plausible, not scheduled for implementation.

## The Idea

Rye already **signs** every item (directive, tool, knowledge) with Ed25519 for integrity. If you **encrypt** them as well, the entire `.ai/` directory becomes an opaque blob — the intelligence exists but is unreadable without keys. Share that encrypted corpus and you get a **cryptographically-gated knowledge fabric**: portable, composable, zero-knowledge to anyone without access.

## Why This is Natural for Rye

Rye treats everything as data — signed, portable files resolved through three spaces (project → user → system). Encryption is one more transformation in the same pipeline:

- The four MCP tools (`search`, `load`, `execute`, `sign`) each get an encryption-aware layer
- The agent still says `execute(tool="utilities/my-tool")` — the framework handles decrypt-in-memory transparently
- No new item types, no new abstractions — just a new property on existing items

---

## Architecture

### Envelope Encryption

A single **AES-256-GCM group key** encrypts all items in a group. That symmetric key is then **wrapped** (encrypted) with each authorized member's Ed25519 public key. This is standard envelope encryption:

- Adding a member = wrap the existing group key with their pubkey (no re-encryption of content)
- Revoking a member = generate a new group key, re-encrypt all items, re-wrap for remaining members
- Multiple groups = multiple independent group keys, items can belong to more than one

### Directory Layout — Partial Encryption

With content encryption but cleartext filenames (leaks structure):

```
.ai/
├── directives/
│   └── core/analyze.md.enc        ← content encrypted, but name reveals intent
├── tools/
│   └── rye/agent/runner.py.enc    ← an observer knows you have a "runner" tool
├── knowledge/
│   └── graphs/state.md.enc
└── keyring/
    ├── groups/
    │   ├── team-alpha.key.enc
    │   └── org-acme.key.enc
    └── grants/
        └── team-alpha/
            ├── bob.key.enc
            └── carol.key.enc
```

### Directory Layout — Full Obfuscation (Vault Mode)

With structure obfuscation, the `.ai/` directory becomes a **flat vault** — no readable filenames, no category folders, no item counts per type. An observer sees nothing:

```
.ai/
├── vault/
│   ├── 7a3f.enc          ← could be a directive, tool, or knowledge entry
│   ├── b91c.enc          ← no way to tell without the group key
│   ├── e204.enc
│   ├── 1d8e.enc
│   └── f6a2.enc
├── vault.idx.enc         ← encrypted index: maps item IDs → vault files
└── keyring/
    ├── groups/
    │   ├── team-alpha.key.enc
    │   └── org-acme.key.enc
    └── grants/
        └── team-alpha/
            ├── bob.key.enc
            └── carol.key.enc
```

The **vault index** (`vault.idx.enc`) is itself encrypted with the group key. It maps logical item IDs to vault filenames and stores all the metadata that would normally be on the filesystem:

```json
{
  "items": {
    "7a3f": {
      "item_type": "tool",
      "item_id": "utilities/data-pipeline",
      "original_path": "tools/utilities/data-pipeline.py",
      "category": "utilities",
      "tags": ["etl", "pipeline"],
      "version": "1.0.0"
    },
    "b91c": {
      "item_type": "directive",
      "item_id": "core/analyze",
      "original_path": "directives/core/analyze.md",
      "category": "core",
      "tags": ["analysis", "code-review"],
      "version": "1.0.0"
    },
    "e204": {
      "item_type": "knowledge",
      "item_id": "graphs/code-review/run-001",
      "original_path": "knowledge/graphs/code-review/run-001.md",
      "category": "graphs/code-review",
      "tags": ["graph_state"],
      "version": "1.0.0"
    }
  },
  "group_id": "team-alpha",
  "created_at": "2026-02-18T10:00:00Z",
  "vault_version": 1
}
```

#### What This Hides

| Without Vault Mode                        | With Vault Mode                                |
| ----------------------------------------- | ---------------------------------------------- |
| File count per type visible               | Total item count visible, types hidden          |
| Category structure visible                | Flat — no categories on disk                   |
| Filenames reveal intent                   | Opaque hex IDs                                 |
| File extensions reveal type (.py, .md)    | All `.enc` — uniform blobs                     |
| Directory depth reveals complexity        | Single `vault/` directory                      |
| Modification times per-item visible       | Still visible per vault file (see Limitations) |

#### How Resolution Works in Vault Mode

Rye's resolver currently walks `directives/`, `tools/`, `knowledge/` directories to find items by ID. In vault mode, the resolver changes to:

1. **Decrypt the vault index** — load `vault.idx.enc`, decrypt with group key, parse JSON
2. **Lookup by item ID** — find the vault filename (e.g., `"utilities/data-pipeline"` → `"7a3f"`)
3. **Decrypt the item** — load `vault/7a3f.enc`, decrypt with group key
4. **Return to executor chain** — the decrypted content is a normal tool/directive/knowledge file

The index is cached in memory for the session, so subsequent lookups don't re-decrypt.

```
agent: execute(tool="utilities/data-pipeline")
  → resolver: decrypt vault.idx.enc (cached after first access)
  → resolver: lookup "utilities/data-pipeline" → vault/7a3f.enc
  → executor: decrypt 7a3f.enc → verify signature → validate chain → run
```

#### Mixed Mode

Not everything needs to be vaulted. A project can have both:

```
.ai/
├── directives/          ← cleartext items (public, unsigned, or locally-signed)
│   └── onboarding.md
├── tools/               ← cleartext tools
│   └── utilities/formatter.py
├── vault/               ← encrypted + obfuscated items
│   ├── 7a3f.enc
│   └── b91c.enc
├── vault.idx.enc
└── keyring/
    └── groups/
        └── team-alpha.key.enc
```

The resolver checks both paths: standard directory resolution first (existing behavior), then vault index lookup if not found. This means adding vault mode is additive — existing items and workflows don't break.

#### Vault Limitations

- **File modification timestamps** are still visible on vault files — an observer can see *when* items were last changed, even if they can't see *what* they are. Mitigation: touch all vault files to the same timestamp on each write, or use a single vault archive file.
- **Item count** is visible (number of files in `vault/`). Mitigation: pad with dummy encrypted files.
- **File sizes** leak information — a 500-byte `.enc` is probably not the same type as a 50KB `.enc`. Mitigation: pad all items to fixed size blocks (e.g., 4KB, 16KB, 64KB buckets).
- **Vault index is a single point** — if it's corrupted, all item resolution breaks. Mitigation: sign the index, keep encrypted backups, or derive index from item headers (each encrypted item could contain its own metadata header after decryption).

### Signature Format: Seal

The existing signature line:

```
rye:signed:TIMESTAMP:HASH:SIG:FP
```

Becomes a **seal** — encrypt-then-sign:

```
rye:sealed:TIMESTAMP:HASH:SIG:FP:GROUP_ID:NONCE
```

| Field      | Purpose                                                         |
| ---------- | --------------------------------------------------------------- |
| `HASH`     | SHA256 of the **ciphertext** (not plaintext)                    |
| `SIG`      | Ed25519 signature of the hash                                   |
| `GROUP_ID` | Which group key encrypts this item                              |
| `NONCE`    | AES-GCM nonce (unique per encryption, required for decryption)  |

The content hash covers the ciphertext, so **integrity verification works without decryption** — you can confirm an item is authentic and untampered without being able to read it.

---

## How It Maps to Existing Components

### Trust Store → Trust Store + Keyring

The existing `~/.ai/trusted_keys/` stores Ed25519 public keys by fingerprint (identity). A new `~/.ai/keyring/` stores group symmetric keys wrapped per-user (access):

| Component               | Purpose                            | Existing? |
| ------------------------ | ---------------------------------- | --------- |
| `~/.ai/trusted_keys/`   | "I trust this key belongs to X"    | ✅ Yes    |
| `~/.ai/keyring/groups/` | "I have access to group Y's items" | New       |
| `~/.ai/keyring/grants/` | "I've granted access to Z"         | New       |

Combined: "I trust this key belongs to Alice (trust store) AND Alice has access to team-alpha's intelligence (keyring)."

### Executor Chain: Decrypt Just-In-Time

The `PrimitiveExecutor.execute()` chain currently does:

```
resolve → verify signature → validate chain → run
```

With encryption:

```
resolve → verify signature (on ciphertext) → decrypt (with group key) → validate chain → run
```

The decrypted content lives **only in memory**, never written to disk. The subprocess primitive receives cleartext via stdin/pipe, not via temp files.

### Registry: Zero-Knowledge Storage

The registry never sees plaintext:

- **Push** sends encrypted items with cleartext metadata (name, category, tags — or these can be separately encrypted for full opacity)
- **Pull** returns encrypted items
- **Search** operates on cleartext metadata fields (or encrypted metadata with client-side search)
- The registry's own signature wraps the ciphertext: `user-encrypted content → registry-signed envelope`

The registry can still enforce namespaces, versioning, visibility, and provenance — all without access to the intelligence itself.

### Knowledge as Encrypted Shared Memory

This is the most interesting consequence. Graph state is already a knowledge item (`.ai/knowledge/graphs/<id>.md`). If encrypted with a group key:

- **Multiple agents** across different machines can read/write shared state — but only if they hold the group key
- **The filesystem/registry** sees only ciphertext — even if storage is compromised, the intelligence (learnings, execution state, domain knowledge) is opaque
- **Knowledge accumulates encrypted** — an organization's entire learned corpus is portable, shareable, and readable only by authorized members

---

## Sharing Model

### Scopes via Group Keys

Different group keys for different access scopes:

| Scope       | Group Key        | What It Covers                                    |
| ----------- | ---------------- | ------------------------------------------------- |
| Personal    | (your own key)   | Your private tools, personal knowledge, learnings |
| Team        | `team-alpha`     | Team's shared workflows, domain knowledge         |
| Org         | `org-acme`       | Organization-wide directives, approved tools      |
| Public      | (no encryption)  | Open-source items, community contributions        |

An item can be encrypted with multiple group keys (encrypted once per group), allowing layered access.

### Workflow: Sharing Encrypted Intelligence

```
1. Alice creates a tool and signs it:
   .ai/tools/utilities/data-pipeline.py

2. Alice seals it with the team key:
   seal(item_id="utilities/data-pipeline", group="team-alpha")
   → .ai/tools/utilities/data-pipeline.py.enc

3. Alice pushes to registry:
   push(item_id="alice/utilities/data-pipeline", ...)
   → Registry stores ciphertext, signs the envelope

4. Bob pulls to his project:
   pull(item_id="alice/utilities/data-pipeline", space="project")
   → .ai/tools/utilities/data-pipeline.py.enc lands in Bob's project

5. Bob executes it:
   execute(tool="utilities/data-pipeline", ...)
   → Executor detects .enc, finds team-alpha key in Bob's keyring
   → Decrypts in memory, verifies inner signature, runs
```

### Key Distribution

Group keys are distributed through the keyring grant mechanism:

1. Alice generates a group key: `create_group(name="team-alpha")`
2. Alice grants Bob access: `grant(group="team-alpha", pubkey_fp="bob-fingerprint")`
   - This wraps the AES group key with Bob's Ed25519 pubkey → `bob.key.enc`
3. Bob receives the wrapped key (via registry, direct file transfer, or any channel)
4. Bob unwraps with his private key → stores in `~/.ai/keyring/groups/team-alpha.key.enc`

The wrapped key file can travel over any channel (email, git, registry) — it's encrypted to Bob's key, so interception is harmless.

---

## What the Agent Experiences

From the LLM's perspective, nothing changes. The four MCP tools work identically:

| Tool      | Without Encryption                  | With Encryption                                          |
| --------- | ----------------------------------- | -------------------------------------------------------- |
| `search`  | Search cleartext metadata           | Search cleartext metadata (or decrypt metadata in-agent) |
| `load`    | Read item content                   | Decrypt + read item content                              |
| `execute` | Verify → run                        | Verify → decrypt → run                                   |
| `sign`    | Sign item                           | Seal item (encrypt + sign)                               |

The agent says `execute(tool="utilities/data-pipeline")` and the framework handles the rest. If the agent doesn't have the group key, execution fails with a clear error — same as an untrusted signature today.

---

## Open Design Questions

### Key Rotation

When a group key is rotated (e.g., after revoking a member):

- **Option A: Re-encrypt everything** — generate new group key, re-encrypt all items, re-wrap for remaining members. Clean but expensive for large corpora.
- **Option B: Epoch-based keys** — items encrypted with old key remain readable (members still hold it), new items use new key. Revoked members can still read old items but not new ones. Simpler but weaker revocation.
- **Option C: Lazy re-encryption** — re-encrypt items on next access/modification. Eventually consistent. Risk: old-key items persist if never accessed.

### Metadata Visibility

What stays in the clear? This interacts directly with vault mode (§ Directory Layout):

- **Partial encryption** (no vault): Filenames and directory structure in the clear, content encrypted. Registry search works. Leaks what you *have*, not what it *does*.
- **Vault mode**: Filenames obfuscated, metadata inside the encrypted index. Registry stores opaque blobs by vault ID. Strongest privacy but requires the group key even to list items.
- **Configurable per-group**: Some groups want full opacity (vault), others want discoverable-but-protected (partial). The `seal` command could accept a `mode: vault | partial` flag.

### Granularity

- **Per-item keys**: Maximum flexibility, key management nightmare.
- **Per-directory keys**: Natural — `.ai/tools/` uses one key, `.ai/knowledge/` uses another. Maps to Rye's space precedence.
- **Per-group keys** (proposed above): One key per access scope. Simplest model, matches organizational boundaries.

### Performance

- AES-256-GCM is fast — encryption/decryption overhead is negligible for typical item sizes (KBs to low MBs)
- The real cost is key management UX: granting, revoking, rotating
- Caching decrypted content in memory for a session avoids redundant decrypts during graph execution

### Thread System Integration

Threads already use Ed25519-signed `thread.json` and checkpoint-signed transcripts. Encrypted threads would mean:

- Transcript JSONL encrypted at rest, decrypted by runner in memory
- Knowledge entries (`.ai/knowledge/agent/threads/`) encrypted — agent learnings are private to the group
- Graph state knowledge items encrypted — execution state visible only to authorized agents

---

## Relationship to Existing Security

| Existing Feature         | What It Does             | Encryption Adds                                      |
| ------------------------ | ------------------------ | ---------------------------------------------------- |
| Ed25519 signing          | Integrity + authenticity | Confidentiality                                      |
| Trust store              | Identity verification    | Access control (keyring)                             |
| TOFU pinning             | Registry key bootstrap   | Group key bootstrap (same pattern)                   |
| Chain verification       | Tamper detection         | Tamper detection on ciphertext (works without decrypt)|
| Capability attenuation   | What agents can do       | What agents can see                                  |

Encryption completes the security model: signing tells you *who* made something and that it hasn't changed; encryption controls *who can read it*.

---

## Summary

Rye's "everything is data" philosophy means encryption is a natural extension — items are already signed portable files, so adding an encrypt layer to the same pipeline gives you:

- **Portable encrypted intelligence**: The `.ai/` folder moves between machines/repos as opaque ciphertext
- **Selective sharing**: Group keys create access boundaries at any scope (personal → team → org)
- **Zero-knowledge infrastructure**: Registry, git, filesystem never see plaintext
- **Structure obfuscation**: Vault mode flattens the directory into opaque blobs — even filenames, item types, and category structure are hidden
- **Composable trust**: Trust store (identity) + keyring (access) = full authorization model
- **Transparent to agents**: The LLM never knows or cares that items are encrypted — the framework handles it
