# Content Integrity Enforcement

> Fixes the complete bypass of signature validation, lockfile integrity, and content
> verification across all 4 MCP tools. No backwards compatibility.

## Problem Statement

The signing infrastructure (MetadataManager, IntegrityVerifier, lockfile system)
exists but is never enforced at execution or loading boundaries.

### Audit

| Component                                                      | Issue                                                                                                                                 |
| -------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- |
| `execute.py` — `_run_directive`, `_run_tool`, `_run_knowledge` | Zero integrity checks before execution                                                                                                |
| `load.py` — `handle()`                                         | Reads/copies files with no verification                                                                                               |
| `search.py` — `_extract_metadata()`                            | No signed/unsigned status in results                                                                                                  |
| `primitive_executor.py` L179-184                               | Lockfile mismatch logs warning, continues execution                                                                                   |
| `integrity_verifier.py`                                        | Dead code — never imported anywhere                                                                                                   |
| Hash scheme split                                              | `MetadataManager` = `sha256(content)`, `IntegrityVerifier`/lockfiles = `compute_tool_integrity(id, version, manifest)` — incompatible |
| `sign.py` batch L136                                           | `file_path.stem` drops relative dirs, breaks `_find_item()`                                                                           |
| `sign.py` glob extensions                                      | Hardcodes `.py` for tools, ignores `get_tool_extensions()`                                                                            |
| `capability_tokens.py` L32                                     | Stores keys in `~/.rye/keys/` — inconsistent with USER_SPACE (`~/.ai/`)                                                               |

---

## Storage Pattern

All state follows the 3-tier USER_SPACE convention (`$USER_SPACE` or `~/.ai/`):

```
~/.ai/                              # USER_SPACE root
├── auth/                           # AuthStore (lilux/runtime/auth.py)
│   ├── .salt
│   └── {service}.enc
├── sessions/                       # Registry device auth sessions
│   └── {session_id}.json
├── lockfiles/                      # LockfileResolver user tier
│   └── {tool_id}@{version}.lock.json
├── keys/                           # Ed25519 keypair (MOVE from ~/.rye/keys/)
│   ├── private_key.pem             # 0600
│   └── public_key.pem              # 0644
├── trusted_keys/                   # Trust store (advanced path)
│   ├── {fingerprint}.pem
│   └── registry.pem                # Pinned registry public key
├── directives/                     # User-space items
├── tools/
└── knowledge/
```

`capability_tokens.py` currently uses `Path.home() / ".rye" / "keys"`. This moves
to `get_user_space() / "keys"` so all state lives under one root.

---

## Fix: Unified Content Integrity

### Design Decisions

- **Single hash scheme**: Use `MetadataManager.compute_hash()` everywhere.
  Delete `integrity_verifier.py` (dead code, incompatible hashes).
- **Lockfiles adopt content hash**: `integrity` field stores the same
  `MetadataManager.compute_hash()` value, not manifest hash.
- **Hard fail**: Missing or invalid signatures block execution and loading.
  System-space items are pre-signed at package time.
- **MCP-native**: All operations go through the 4 MCP tools.
  No CLI commands. Key management is via the `sign` tool or `execute` with
  a key-management directive.

### New Module: `rye/rye/utils/integrity.py`

```python
from rye.utils.metadata_manager import MetadataManager

class IntegrityError(Exception):
    """Content integrity check failed."""
    pass

def verify_item(file_path: Path, item_type: str, *, project_path: Path = None) -> str:
    """Verify signature matches content. Returns verified hash.

    Raises IntegrityError if unsigned or tampered.
    """
    content = file_path.read_text(encoding="utf-8")

    sig_info = MetadataManager.get_signature_info(
        item_type, content, file_path=file_path, project_path=project_path
    )
    if not sig_info:
        raise IntegrityError(f"Unsigned item: {file_path}")

    expected = sig_info["hash"]
    actual = MetadataManager.compute_hash(
        item_type, content, file_path=file_path, project_path=project_path
    )
    if actual != expected:
        raise IntegrityError(
            f"Integrity failed: {file_path} "
            f"(expected {expected[:16]}…, got {actual[:16]}…)"
        )
    return actual
```

### Changes by MCP Tool

#### `execute.py`

Call `verify_item()` before every execution path. `IntegrityError` falls through
to the existing `except Exception` in `handle()` → returns `{"status": "error"}`.

- `_run_directive`: verify before parse
- `_run_knowledge`: verify before parse
- `_run_tool`: verification happens in PrimitiveExecutor (below)

#### `primitive_executor.py`

**a) Verify every chain element after build, before execution:**

```python
from rye.utils.integrity import verify_item
from rye.constants import ItemType

# After _build_chain, before _validate_chain:
for element in chain:
    verify_item(element.path, ItemType.TOOL, project_path=self.project_path)
```

**b) Lockfile mismatch blocks execution:**

Replace L179-184 warning with error return:

```python
if lockfile.root.integrity != current_integrity:
    return ExecutionResult(
        success=False,
        error=f"Lockfile integrity mismatch for {item_id}. Re-sign and delete stale lockfile.",
    )
```

**c) Lockfile integrity uses content hash:**

Replace all `compute_tool_integrity(tool_id, version, manifest)` calls with
`MetadataManager.compute_hash(ItemType.TOOL, content, file_path=path, project_path=...)`.

#### `load.py`

Call `verify_item()` after resolving source path, before reading content or copying.

#### `search.py`

Add `signed` and `integrity` fields to result metadata. Read-only — no blocking.
Use a try/except around `verify_item()` to avoid failing the entire search if one
item is unsigned.

#### `sign.py` — Bug Fixes

**a) Batch: preserve relative path ID:**

```python
# Replace: item_id = file_path.stem
# With:
item_id = str(file_path.relative_to(base_dir).with_suffix(""))
```

Pass `base_dir` from `_resolve_glob_items` into `_sign_batch`.

**b) Batch: use `get_tool_extensions()`:**

Replace hardcoded `.py` in `_resolve_glob_items` for tools with the data-driven
extension list.

### Delete `integrity_verifier.py`

Remove `rye/rye/executor/integrity_verifier.py`. Never called, incompatible hash
scheme, replaced by `verify_item()`.

### Lockfile Hash Migration

Replace `compute_tool_integrity()` imports in `primitive_executor.py` with
`MetadataManager.compute_hash()`. Delete existing lockfiles — they regenerate
with content hashes on next successful execution.

---

## Advanced Path: Ed25519 Authenticated Signatures

Current scheme = tamper evidence (content hash). Anyone with write access can
re-sign. This path adds Ed25519 for **authenticity** (who signed it).

This is not a v2 format — it replaces the current format entirely. The old
`rye:validated:` format is rejected after migration.

### Trust Model

```
LOCAL (offline)                    REGISTRY (online)
┌──────────────┐                   ┌──────────────────┐
│ User keypair │                   │ Registry keypair │
│ ~/.ai/keys/  │                   │ (server-side)    │
│ Ed25519      │                   │ Ed25519          │
└──────┬───────┘                   └────────┬─────────┘
       │                                    │
  signs locally                        signs on push
  (self-signed)                       (attests provenance)
       │                                    │
       ▼                                    ▼
┌───────────────┐                   ┌──────────────────┐
│ rye:signed    │                   │ rye:signed       │
│ hash + Ed25519│                   │ hash + Ed25519   │
│ + pubkey fp   │                   │ + |reg@user      │
└───────────────┘                   └──────────────────┘

Trust store: ~/.ai/trusted_keys/
 - Own pubkey (auto-trusted on keygen)
 - Registry pubkey (pinned on first pull, TOFU)
 - Peer pubkeys (manually trusted via sign tool)
```

### Signature Format

One format. No versioning. Ed25519 is always present. When no keypair exists,
`sign` auto-generates one on first use (same as `capability_tokens.ensure_keypair()`).

```
<!-- rye:signed:TIMESTAMP:CONTENT_HASH:ED25519_SIG:PUBKEY_FP -->
# rye:signed:TIMESTAMP:CONTENT_HASH:ED25519_SIG:PUBKEY_FP
```

- `CONTENT_HASH` — SHA256 of content (computed by MetadataManager)
- `ED25519_SIG` — base64url signature over `CONTENT_HASH`
- `PUBKEY_FP` — first 16 hex chars of SHA256(public_key) (fingerprint)

Registry-attested appends provenance: `...:FP|rye-registry@username`

### Pure Offline / No Auth / Local Signing

There is no "unsigned but hash-stamped" mode. Every signature includes Ed25519.
Local offline flow:

1. User (or agent) calls `rye_sign` → `SignTool.handle()`
2. `SignTool` calls `MetadataManager.sign_content()` which now:
   a. Computes content hash (same as before)
   b. Loads local keypair from `get_user_space() / "keys/"` (auto-generates if missing)
   c. Signs the content hash with Ed25519 private key
   d. Embeds `HASH:SIG:FP` in the signature comment
3. No network, no auth, no registry. Just local keypair.

On verify (`verify_item()`):

1. Extract `CONTENT_HASH`, `ED25519_SIG`, `PUBKEY_FP` from signature
2. Recompute content hash — must match `CONTENT_HASH`
3. Look up `PUBKEY_FP` in trust store (`~/.ai/trusted_keys/`)
4. Verify `ED25519_SIG` against that public key

The local user's own public key is auto-added to the trust store on keygen, so
self-signed items always verify locally without any manual trust step.

### Offline Operation

Everything works offline:

- **Signing** — local keypair, no network
- **Verification** — local trust store lookup
- **Execution** — all checks are local hash + signature verify
- **New items** — sign locally, execute locally, push when online

Network required only for: `registry push`, `registry pull`, `registry search`,
first-time registry key pinning (TOFU on first pull).

### Agent Harness Integration

The agent thread system (`rye/agent/threads/`) has two integrity gaps that this
fix closes.

**Architecture context:** The harness flow is:

```
thread_directive.execute()
  → _load_directive()                # loads from .ai/directives/ — NO verification
  → _mint_token_from_permissions()   # mints CapabilityToken from directive perms
  → SafetyHarness(parent_token=token)
  → _run_tool_use_loop()
      → _call_llm()                  # LLM call via provider tool
      → _execute_tool_call()         # routes to primary tools via PrimitiveExecutor
          → PrimitiveExecutor.execute(item_id="rye/primary-tools/rye_execute")
              → rye_execute.execute() → ExecuteTool.handle()
```

The 4 primary tools (`rye/primary-tools/rye_{execute,load,search,sign}.py`) are
thin wrappers that delegate to `rye.tools.{execute,load,search,sign}`. They are
the only tools the harness exposes to the LLM, resolved from directive permissions
via `_resolve_tools_for_permissions()`.

**Gap 1 — Directive loading has no integrity check:**

`thread_directive._load_directive()` → `DirectiveHandler.resolve()` → `parse()`
reads and parses a directive with no signature verification. A tampered directive
gets full execution with whatever permissions it declares — it can mint its own
CapabilityToken via `_mint_token_from_permissions()`.

Fix: `_load_directive()` calls `verify_item(file_path, ItemType.DIRECTIVE)` before
parsing. Failure returns `{"status": "failed", "error": ...}`.

**Gap 2 — Primary tools execute without integrity check:**

`_execute_tool_call()` → `PrimitiveExecutor.execute()` builds the chain and runs.
Fixed by the PrimitiveExecutor changes above (verify every chain element before
execution).

The harness does not need its own separate integrity layer — enforcement comes
from:

- `_load_directive()` verifying the directive on load
- `PrimitiveExecutor` verifying every tool in the chain
- Primary tool wrappers inheriting enforcement from the core tools they delegate to

**CapabilityToken signing uses same keypair:**

`capability_tokens.py` currently stores keys at `Path.home() / ".rye" / "keys"`
(inconsistent with USER_SPACE). Move to `get_user_space() / "keys/"`. Single
Ed25519 keypair for both capability tokens and content signing — one identity.

`_mint_token_from_permissions()` → `ensure_keypair()` → `sign_token()` already
uses Ed25519. After the key path migration, both systems use the same key.

### Key Management (via MCP tools)

All key operations go through MCP tools:

**Via `sign` tool (`rye_sign`):**

- `sign(item_type, item_id)` — signs with local keypair (auto-generates if missing)
- `sign(item_type, "*")` — batch sign all items of a type

**Via `execute` tool (`rye_execute`) with trust directives:**

- `execute(directive, "trust/add", parameters={...})` — trust a peer key
- `execute(directive, "trust/list")` — list trusted keys
- `execute(directive, "trust/remove", parameters={...})` — revoke trust

**Via `registry` tool (existing):**

- `push` — sends locally-signed content, server re-signs with registry key
- `pull` — verifies registry signature against pinned key, pins on first pull (TOFU)

### Verification Levels

| Level               | Meaning                                        | Source       |
| ------------------- | ---------------------------------------------- | ------------ |
| `self-signed`       | Signed by local keypair, pubkey in trust store | Local items  |
| `registry-attested` | Signed by registry, registry pubkey pinned     | Pulled items |
| `peer-trusted`      | Signed by manually trusted key                 | Shared items |
| `unsigned`          | No signature                                   | **Rejected** |
| `untrusted`         | Signed but pubkey not in trust store           | **Rejected** |

### Implementation Phases

**Phase 1 — Core signing primitives (lilux layer):**

`lilux/lilux/primitives/signing.py`:

- `sign_hash(content_hash, private_key) -> base64url_sig`
- `verify_signature(content_hash, signature, public_key) -> bool`
- `compute_key_fingerprint(public_key) -> str` (16 hex chars)

**Phase 2 — Trust store (rye layer):**

`rye/rye/utils/trust_store.py`:

- `TrustStore(trust_dir=get_user_space() / "trusted_keys")`
- `is_trusted(fingerprint) -> bool`
- `add_key(public_key_pem, label) -> fingerprint`
- `get_key(fingerprint) -> Ed25519PublicKey`
- `pin_registry_key(public_key_pem) -> fingerprint`
- `get_registry_key() -> Optional[Ed25519PublicKey]`

**Phase 3 — Unified MetadataManager signing:**

`MetadataManager.sign_content()` becomes Ed25519-aware:

- Loads keypair from `get_user_space() / "keys/"` (auto-generates via `ensure_keypair()`)
- Produces `rye:signed:T:HASH:SIG:FP`
- `MetadataManager.verify_content()` checks hash + Ed25519 + trust store

`verify_item()` in `integrity.py` calls `MetadataManager.verify_content()`.

**Phase 4 — Key path migration:**

Move `capability_tokens.py` `DEFAULT_KEY_DIR` from `Path.home() / ".rye" / "keys"`
to `get_user_space() / "keys"`. Both capability tokens and content signing share
one Ed25519 keypair — single identity under `~/.ai/keys/`.

**Phase 5 — Registry integration:**

- `registry push`: server strips local sig, re-signs with registry Ed25519 key
- `registry pull`: client verifies against pinned registry key
- Server exposes `GET /v1/public-key` for TOFU pinning
- `_pull()` checks trust store for pinned key, pins on first pull

**Phase 6 — Agent harness hardening:**

- `thread_directive._load_directive()` calls `verify_item()` before parsing
- Primary tools in `rye/primary-tools/` inherit enforcement from the core tools
  they delegate to — no changes needed in the wrappers themselves
- `capability_tokens.ensure_keypair()` uses new key path

### Migration

1. Delete all existing lockfiles (`~/.ai/lockfiles/`, `{project}/lockfiles/`)
2. Re-sign all items via `sign` tool (batch: `sign(item_type, "*")`)
3. `registry push` re-signs with registry key
4. Old `rye:validated:` signatures are rejected — items must be re-signed
