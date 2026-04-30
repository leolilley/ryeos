# Principal-scoped vault reads

**Status:** deferred. Tracked by plan §259.

## Current behavior

The daemon vault (`SealedEnvelopeVault`, X25519 sealed-envelope, XChaCha20-Poly1305) holds a flat `BTreeMap<String, String>`. `read_required_secrets(declared)` returns every declared name that's in the store. There is no per-entry principal scoping — any item that declares a secret name in `required_secrets` and is allowed to dispatch can receive that secret's value.

## Why this is OK for v1 dogfood

Single-operator scenario. The operator put the secret in the vault; they implicitly trust every item they cause to dispatch. Per-item declared-secret gating (already shipped via `required_secrets`) is sufficient to prevent leakage to undeclared items.

## What principal scoping would add

Each `VaultEntry` declares which principals can read it:

```rust
pub struct VaultEntry {
    pub value: String,
    /// Fingerprints of principals (compute_fingerprint outputs)
    /// allowed to read this entry. Empty Vec = refused for all.
    pub principals: Vec<String>,
}
```

Read API: `read_required_secrets(principal_fp, declared)` — partial denial is a hard fail; undeclared scope is a hard fail.

CLI: `rye vault put NAME=VALUE --principal <fp>` mandatory; no any-principal fallback.

## When to revisit

- Multi-operator scenarios (federation, shared daemon, ops team vs developer split).
- Defense-in-depth need: limit blast radius of a compromised item that legitimately runs but shouldn't see all secrets.
- Auditability: vault entries with explicit scopes give clearer logs of "who could read what."

## Implementation cost

- `lillux::vault` sealed-envelope schema bump (storage format change).
- `ryeosd` vault trait API: `read_required_secrets(principal, declared)` signature change.
- `ryeos-cli` `rye vault put` mandatory `--principal <fp>` flag + `user`/`node` shorthands that resolve fingerprints from disk.
- Existing operator vault entries become incompatible with no migration shim → wipe-and-recreate.
- All existing tests that call `vault put` need scope updates.

Roughly 4 crates touched, format break, mandatory CLI flag, operator wipe. Bounded but non-trivial.

## Cross-references

- `docs/future/key-rotation-and-trust-policy.md` — broader trust/key lifecycle posture.
- `.tmp/POST-KINDS-FLIP-PLAN.md` §259 — original deferral note.
