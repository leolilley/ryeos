# Mental Model

Rye OS in five concepts.

## 1. Items

Everything the system operates on is an **item**: a signed file in a well-known directory.

| Type | What it is | Extension | Lives in |
|------|-----------|-----------|----------|
| Directive | Workflow instructions ("how to do X") | `.md` (YAML frontmatter) | `.ai/directives/` |
| Tool | Executable script ("do X") | `.py`, `.sh`, `.rs` binary | `.ai/tools/` |
| Knowledge | Domain information, context | `.md` | `.ai/knowledge/` |
| Config | Runtime configuration | `.yaml` | `.ai/config/` |

Items are addressed by **canonical ref**: `kind:path/without/ext`. Examples:

- `directive:ryeos/core/init` → `.ai/directives/ryeos/core/init.md`
- `tool:ryeos/core/identity/public_key` → `.ai/tools/ryeos/core/identity/public_key.py`
- `knowledge:ryeos/core/signing` → `.ai/knowledge/ryeos/core/signing.md`

## 2. Spaces

Items resolve through three spaces, first match wins:

```
project  →  user  →  system
```

| Space | Path | What lives there |
|-------|------|-----------------|
| **Project** | `.ai/` in the project root | Project-specific directives, tools, knowledge |
| **User** | `~/.ai/` | Cross-project personal items, signing keys, trust store |
| **System** | `$XDG_DATA_DIR/ryeos/` | Core bundle: kind schemas, parsers, handlers, protocols |

When you execute `directive:my/workflow`, the engine searches project `.ai/directives/`, then `~/.ai/directives/`, then the system bundle.

## 3. Bundles

A **bundle** is a directory tree with a signed manifest that the daemon registers and indexes. Bundles extend the system with new item types, handlers, and runtimes.

- **Core bundle** — kind schemas, parser tools, subprocess handlers, protocols. Installed by `ryeos init`.
- **Standard bundle** — directives, tools, knowledge for everyday use. Registered during init.

Bundles are **content-addressed**: every file is stored as a CAS blob, and the manifest records the hash. The daemon verifies signatures and hashes before admitting any bundle content.

## 4. Threads

A **thread** is a running execution. When you execute a directive, the daemon:

1. Resolves the item through spaces
2. Verifies its signature and hash
3. Builds an execution plan (which handler, which runtime, which protocol)
4. Launches a subprocess (the "runtime") with an **envelope** of context
5. The runtime calls back to the daemon for sub-actions (tool dispatch, state reads/writes)

Threads have persistent state stored in an append-only CAS chain. This means:
- State survives process crashes
- Threads can be resumed from checkpoints
- Every state transition is signed and auditable

## 5. Trust

Every signable item must carry an Ed25519 signature in a `# ryeos:signed:...` header. The trust model is:

- **Node key** — the daemon's identity, generated at init
- **User key** — the operator's identity, generated at init
- **Trusted signers** — pinned public keys in `~/.ai/config/keys/trusted/`

At boot, the daemon loads the trust store and verifies every bundle, kind schema, handler, and node-config item against it. Untrusted items are rejected — there is no "trust on first use" or soft fallback.

## Putting it together

```
You type:  ryeos execute directive:my/workflow with {"target": "prod"}

Daemon:
  1. Resolve my/workflow through project → user → system spaces
  2. Verify signature against trust store
  3. Look up the directive's kind schema → handler → runtime → protocol
  4. Build an envelope (item content, inputs, signing identity, callback endpoint)
  5. Spawn ryeos-directive-runtime as a subprocess
  6. Runtime reads envelope, starts LLM loop, calls back for tool dispatches
  7. Each tool dispatch: resolve → verify → spawn → collect result
  8. State transitions written to CAS chain, signed with node key
  9. Thread completes → final state → response to CLI
```

## See also

- [Canonical Refs](canonical-refs.md) — full ref syntax
- [Spaces](spaces.md) — space resolution details
- [Trust and Signing](trust-and-signing.md) — signature format, key management
- [Threads](threads.md) — thread lifecycle and state model
