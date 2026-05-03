# MCP per-request auth

**Tracked from:** `02-ENFORCEMENT-GAPS.md` §4; Phase 2 commit δ
(threat-model docs landed; this is the follow-up).

**Why deferred:** Real per-request auth needs a defined transport
channel and signed-request handshake. Adding an env-var shared
secret without a defined per-call request channel is configuration
theatre. The user is single-operator on localhost today, so the
threat model docs + posture are sufficient until collaborators are
invited.

## Design space

The `rye_signed` HTTP auth verifier already exists for daemon routes
(`ryeosd/src/auth.rs::verify_request`). MCP per-request auth should
mirror that mechanism:

1. **Per-request signature** using the operator's user signing key.
2. **Audience binding**: signature is bound to the MCP server's
   identity (e.g., `fp:<node_fp>` or a dedicated MCP audience tag).
3. **Replay protection**: per-call nonce + timestamp window
   (mirroring `TIMESTAMP_MAX_AGE_SECS = 300` in the HTTP auth path).
4. **Verification at MCP entry**: signature valid, signer trusted by
   the daemon's trust store, scopes sufficient for the requested
   verb.

## Integration options

**(a) Forward signed request to daemon.** MCP server becomes a thin
transport adapter: it receives the signed request, hands it to the
daemon over UDS, daemon verifies via the existing `verify_request`
path. Single source of auth truth.
- Pro: reuses all existing verification logic.
- Con: requires re-architecting the MCP server as a transport-only
  adapter (currently it shells out to the `rye` CLI).

**(b) Verify signature in MCP server itself.** MCP server fetches a
trust-store snapshot from the daemon and verifies signatures locally.
- Pro: more decoupled.
- Con: duplicates verification logic; trust store sync is its own
  problem (when does the MCP server refresh its snapshot?).

**Recommendation:** (a). Couples the MCP server to the daemon's UDS
transport, but the alternative duplicates security-critical code,
which is the wrong trade-off.

## Wire format proposal

MCP request envelope adds:
```python
{
    "tool": "cli",
    "args": {...},
    "auth": {
        "key_id": "fp:<fingerprint>",
        "timestamp": "<unix-seconds>",
        "nonce": "<base64-random>",
        "signature": "<base64-ed25519-sig>",
        "audience": "ryeos-mcp:<node-fp>",
    }
}
```

Signature payload:
```
ryeos-mcp-request-v1
<tool-name>
<canonical-args-json>
<timestamp>
<nonce>
<audience>
```

## Capability gating

Once auth is in place, gate specific CLI verbs behind capability
checks at the MCP layer:
- `sign` requires proof of principal identity.
- `execute` passes through caller scopes.
- `vault rewrap` / `daemon rotate-key` require explicit
  high-privilege scope.

## Wave shape (when scheduled)

Two commits:
1. `feat(mcp): per-request signed auth handshake (option A — forward to daemon)`
2. `feat(mcp): capability gating per CLI verb`
