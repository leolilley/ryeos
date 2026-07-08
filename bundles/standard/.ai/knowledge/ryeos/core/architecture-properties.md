<!-- ryeos:signed:2026-06-24T04:51:58Z:0d6f7ea67c8f4c77017054e25e3cd7812a55f2a87496e140c867c25ea28b8bd1:9BKwtoks+YFQkXW8BWCf3UpvoHcBI7hOROgbgsrBsoB76ZCYfnBCs6JwsWProrKH2sOUumVrMZS60xKq1+ZDBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
category: ryeos/core
tags: [architecture, properties, emergent, design, reference]
version: "1.0.0"
description: >
  A catalogue of the architectural properties that emerge from Rye OS's
  foundation of content-addressed, signed, capability-gated execution.
  Each property links to its detailed knowledge entry.
---

# Architectural Properties

Rye OS has a set of properties that emerge from its foundation rather
than being bolted on as features. The common thread is **CAS-as-truth**
— once you have content-addressed immutable storage with cryptographic
signing, everything else becomes a layer on top that inherits those
properties for free.

This page is a map. Follow the links for the full treatment of each
property.

## The Properties

### 1. Self-Authenticating File Format

Every item carries its own authentication in a single line:

```
```

You don't need a manifest, lockfile, or side-channel to know if a file
was tampered with. The file tells you itself. This applies to routes,
verbs, kind schemas, parsers, services, and tool definitions — the
entire configuration surface is self-authenticating.

→ [Signing and Trust](signing.md)

### 2. Zero-Deployment CLI Extensibility

The CLI has no verb table in code. It sends raw token arrays to the
daemon, which resolves aliases against the bundle registry. Install a
bundle → new verbs appear instantly. No CLI redeployment. The MCP
server inherits this for free.

→ [CLI Reference](node/cli-reference.md), [Spaces](spaces.md)

### 3. CAS-as-Truth with Rebuildable Projection

Three-tier truth: immutable CAS objects → signed refs → rebuildable
SQLite projection. The projection is a materialized view that can be
deleted and fully recovered. CAS-first writes mean projection failures
are non-fatal.

→ [CAS Architecture](state/cas-architecture.md)

### 4. Dual Hash-Linked Event Braid

Events maintain two hash links — `prev_chain_event_hash` (global
ordering) and `prev_thread_event_hash` (per-thread sub-chain). Efficient
global replay AND per-thread replay without duplicating events.

→ [Event Braid](state/event-braid.md)

### 5. DAG Versioning with Topological Compaction

Snapshots form a directed acyclic graph with `parent_hashes: Vec<String>`.
GC compaction uses Kahn's algorithm for topological sort when pruning
and rewriting history. The right model for distributed write sources.

→ [DAG Versioning](state/dag-versioning.md)

### 6. Trust Fold-Back (Weakest-Link Inheritance)

Trust propagates through extends chains by taking the minimum:
TrustedBundle > TrustedProject > UntrustedProject > Unsigned. A single
untrusted ancestor taints the entire chain. Source space caps trust level
— a bundle-signed binary reached through a project-tier descriptor is
capped at TrustedProject.

→ [Signing and Trust](signing.md), [Permissions](permissions.md)

### 7. Per-Request Engine Overlays

Remote execution materializes the caller's project source, parsers,
kind schemas, trust pins, and tool definitions into a per-request engine
overlay. The remote adapts to the caller's context — the entire
execution context travels with the push.

→ [Remote Operations](remote/remote-operations.md)

### 8. Data-Driven Route System

The entire HTTP surface is 16 signed YAML files compiled at startup into
a single axum fallback. Five response modes, eight invoker types,
per-route semaphores, auth as a route property, and ArcSwap hot-reload.

→ [Route System](node/routes.md)

### 9. Hermetic Handler Execution

Handler binaries run with `env_clear()` — a completely scrubbed
environment. No secret leakage, no non-determinism, no
environment-dependent behavior.

→ [Execution Isolation](engine/isolation.md)

### 10. Signed Configuration as Compilation

Signing gives you what a compiler gives you (integrity, structural
validation, cross-reference checks) plus provenance, trust gating, and
runtime verification. It applies to configuration, not just code, and
spans the trust boundary — the "standard library" is subject to the
same verification as any third-party extension.

→ [Signing and Trust](signing.md), [Route System](node/routes.md)

### 11. Capability System with Canonical Wire Format

Four-slot wire format `ryeos.<verb>.<kind>.<subject>` validated at
write, load, and check time. AND-of-ORs authorization policies.
Directional wildcards. No auto-prefixing — prevents inert auth.

→ [Permissions](permissions.md)

### 12. Callback Capability Propagation

`effective_caps` compose from the kind's permission model, propagate to
callback tokens, and survive daemon restart via `ResumeContext`. Children
cannot escalate beyond their parent's caps.

→ [Execution Isolation](engine/isolation.md),
  [Callback Authentication](protocols/callback-auth.md)

### 13. Sealed Envelope Vault

X25519 + XChaCha20-Poly1305 with a two-layer construction (AEAD data
encryption + DEK-wrap to vault key). Per-remote key pinning. Vault key
rotation does not affect sealed secrets.

→ [Identity Model](identity-model.md)

### 14. SQLite Schema Ownership

Every database is stamped with a `PRAGMA application_id`. On open,
exhaustive verification checks tables, columns, types, primary keys, and
indexes. Foreign files produce a specific error with recovery
instructions.

→ [CAS Architecture](state/cas-architecture.md)

## The Meta-Observation

These properties emerge from the CAS-as-truth foundation rather than
being independently designed. The system set out to be an AI workflow
runner and ended up solving:

- **Package management** (bundles with kind dependencies)
- **Configuration management** (signed, versioned, content-addressed)
- **Secret management** (per-remote sealed envelope vault)
- **Remote execution** (portable engine overlays, Ed25519 transport)
- **Data backup/sync** (CAS push/pull with incremental dedup and GC)
- **Process supervision** (subprocess lifecycle, cancellation, resume)
- **Access control** (capability-based, per-route, canonical validation)
- **Audit logging** (dual hash-linked event braid with durability tiers)
