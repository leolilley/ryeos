# Security Overview

RYE enforces a mandatory signing model: every item — directive, tool, and knowledge entry — must carry a valid Ed25519 signature before it can be executed, loaded, or used. Unsigned items are rejected. There is no fallback and no bypass.

## Signature Requirement

All items pass through `verify_item()` ([`rye/rye/utils/integrity.py`](rye/rye/utils/integrity.py)) before use. This function checks that:

1. A signature exists in the `rye:signed:` format
2. The embedded content hash matches the actual content
3. The Ed25519 signature is cryptographically valid
4. The signing key is present in the local trust store

Any failure raises `IntegrityError` and halts the operation.

## Trust Levels

RYE recognizes three trust levels:

| Level                 | Description                                                                            | Key Storage                            |
| --------------------- | -------------------------------------------------------------------------------------- | -------------------------------------- |
| **Self-signed**       | Signed with the user's local Ed25519 keypair. Auto-trusted on generation.              | `~/.ai/keys/`                          |
| **Registry-attested** | Re-signed by the registry's Ed25519 key on push. Verified via TOFU-pinned key on pull. | `~/.ai/trusted_keys/registry.pem`      |
| **Peer-trusted**      | Third-party public key manually added to the trust store.                              | `~/.ai/trusted_keys/{fingerprint}.pem` |

## Threat Model

| Threat                        | Mitigation                                                                                             |
| ----------------------------- | ------------------------------------------------------------------------------------------------------ |
| Content tampering             | SHA256 content hash embedded in signature; recomputed and compared on every verification               |
| Chain element drift           | Per-element integrity hashes in lockfiles detect changes to any tool in the resolution chain           |
| Dynamic dependency tampering  | `verify_dependency()` calls `verify_item()` before any load of files under `.ai/tools/` (all file types) |
| Bundle asset tampering        | Signed bundle manifest covers all files (including non-code assets) with per-file SHA256 hashes        |
| Provenance forgery            | Ed25519 signatures bind content to a specific keypair; key must be in trust store                      |
| Capability escalation         | Capability tokens scoped to directive-declared permissions; child threads can only narrow, never widen |
| Prompt/shell injection        | Two-pass template substitution with `shlex.quote()` escaping for all shell-unsafe values               |
| Registry key rotation attacks | TOFU pinning — first-seen registry key is stored; subsequent keys ignored                              |
| Legacy format bypass          | Old `rye:validated:` and `kiwi-mcp:validated:` signatures are rejected outright                        |

## Related Documents

- [Content Signing](content-signing.md) — signature format, verification flow, enforcement points, lockfile chain integrity, dynamic import verification, bundle manifests
- [Keys and Trust](keys-and-trust.md) — keypair management, trust store API, cryptographic primitives
- [TOFU Registry Pinning](tofu-registry-pinning.md) — registry key pinning and provenance verification
- [Capability Tokens](capability-tokens.md) — permission declarations, token minting, attenuation
- [Agent Thread Safety](agent-thread-safety.md) — safety harness, limit enforcement, runtime guards
- [Injection Hardening](injection-hardening.md) — template substitution, shell escaping, input sanitization
- [Lockfile Format](../reference/file-formats/lockfile-format.md) — lockfile structure, per-element integrity, portability
- [App Bundling and Orchestration](../concepts/app-bundling-and-orchestration.md) — two-layer signing model, bundle manifests, verified loader, LLM-orchestrated applications
