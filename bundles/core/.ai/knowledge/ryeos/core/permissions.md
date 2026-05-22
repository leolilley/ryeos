
---
category: ryeos/core
tags: [fundamentals, permissions, capabilities, security, authorization]
version: "2.0.0"
description: >
  How capabilities and permissions work in Rye OS — the canonical
  wire format, validation at three enforcement levels, AND-of-ORs
  authorization policies, directional wildcards, capability propagation,
  and node-signed authorized keys.
---

# Permissions and Capabilities

Rye OS uses **capability-based access control** to gate execution.
Every tool call and service invocation is checked against the calling
context's permission set.

## Canonical Wire Format

Every capability string follows a strict four-slot grammar:

```
ryeos.<verb>.<kind>.<subject>
```

| Slot | Semantics | Constraints |
|---|---|---|
| `verb` | What action | Kebab-case only (no underscores). Known verbs: `execute`, `fetch`, `sign`. |
| `kind` | What resource type | Kebab-case only (no underscores). E.g. `service`, `tool`, `directive`. |
| `subject` | Which specific resource | May contain `/`, `.`, `_`, `-`, `0-9`, `a-z`. E.g. `bundle.install`, `ryeos/file-system/read`. |

Examples:

```
ryeos.execute.service.bundle.install
ryeos.execute.tool.ryeos/file-system/read
ryeos.fetch.tool.ryeos/core/sign
ryeos.sign.directive.my/workflow
```

## Three-Level Validation

Capability strings are validated at three enforcement points to prevent
the worst failure mode: **inert auth** — where a request authenticates
successfully but silently authorizes nothing.

### 1. Write Time

When an authorized-key TOML is written (e.g. via `ryeos authorize-key`):

- Every scope is passed through `validate_scope_pattern`
- Short-form scopes like `bundle.install` (missing `ryeos.` prefix) are
  rejected with an explicit message
- Underscores in the verb or kind slot are rejected
- Uppercase, spaces, and non-ASCII characters are rejected
- Wildcard `*` is rejected unless the caller is the local operator
  (`WildcardPolicy`)

### 2. Load Time

When an authorized-key TOML is loaded from disk:

- Every scope is re-validated via `validate_scope_pattern`, even if the
  file was written by the canonical writer. This catches hand-crafted
  TOML files that bypass the writer.
- The signer fingerprint is verified against the node's own key — only
  the node that created the file can have signed it.

### 3. Check Time

When a capability is actually checked at runtime:

- Required patterns are matched against granted scopes
- Both granted-side and required-side wildcards are supported (see below)
- Matching uses structured field comparison with regex fallback

## Authorization Policies

Authorization uses an **AND-of-ORs** model:

```rust
AuthorizationPolicy::Protected {
    all_of: vec![
        CapabilityClause { any_of: ["ryeos.execute.service.a", "ryeos.execute.tool.b"] },
        CapabilityClause { any_of: ["ryeos.execute.service.c"] },
    ],
}
```

Every clause must be satisfied (AND). Within a clause, any one scope is
sufficient (OR).

- `require("ryeos.execute.service.bundle.install")` — single-clause
  shorthand
- `require_all(&["cap_a", "cap_b"])` — N clauses, each with one
  element; all must match
- `require_all(&[])` — zero clauses, trivially satisfied (no requirements)

## Wildcard Semantics

Wildcards are **directional** — they behave differently depending on
which side they appear.

### Granted-Side Wildcards

A broad grant covers narrow requirements:

| Granted | Satisfies Required |
|---|---|
| `ryeos.execute.service.*` | `ryeos.execute.service.bundle.install` |
| `ryeos.execute.*` | `ryeos.execute.service.bundle.install` |
| `ryeos.*` | Any `ryeos.*` cap |
| `*` | Everything (superuser) |

### Required-Side Wildcards

A broad requirement is satisfied by any narrow grant:

| Required | Satisfied By |
|---|---|
| `ryeos.execute.service.*` | `ryeos.execute.service.bundle.install` |
| `ryeos.execute.*` | `ryeos.execute.service.bundle.install` |
| `ryeos.*` | Any `ryeos.*` grant |

### Path-Prefix Wildcards

A prefix wildcard matches the dot boundary:

- `ryeos.execute.service.bundle.*` matches `bundle.install` ✓
- `ryeos.execute.service.bundle.*` matches `bundleX` ✗ (dot is literal)

The `*` becomes `.*` in regex, and the preceding dot is escaped, so
prefix wildcards don't leak across segments.

### Verbs Are Independent

`ryeos.execute.service.x` does **NOT** imply `ryeos.fetch.service.x`.
Every verb is an independent namespace. If you want multiple verbs, you
must grant each one explicitly.

## Capability Propagation

Capabilities flow through the system in a specific chain:

### 1. Authorized Key → Principal

When a signed request arrives, the daemon loads the authorized-key TOML
for the signing key's fingerprint. The scopes in that file become the
principal's capabilities. No transformation, no filtering, no
augmentation.

### 2. Principal → Thread Launch

The principal's scopes are checked against the service/directive's
required capabilities. If the policy is satisfied, the thread is
launched.

### 3. Thread Launch → Effective Caps

The engine composition pipeline (kind handlers like `extends-chain` and
`graph-permissions`) lifts `permissions.execute` declarations into
`effective_caps` — the composed capability set for the launched item.
This is what the item is allowed to dispatch back to the daemon.

### 4. Effective Caps → Callback Tokens

When the daemon mints a callback token for the subprocess, the
`effective_caps` are copied onto the token. When the child process
calls back, the daemon enforces these caps before dispatch. The child
cannot escalate beyond what the parent allowed.

### 5. Effective Caps → Resume

The `effective_caps` are persisted in `ResumeContext` in the runtime
database. If the daemon restarts and auto-resumes a thread, the resumed
process gets a fresh callback token with the **same** caps — it doesn't
inherit the restart-time principal's capabilities.

## Node-Signed Authorized Keys

Authorized-key TOML files are **node-signed**, not self-signed. The
daemon verifies:

2. The content hash matches
3. The signer fingerprint matches the node's own key — only the node
   that created the file can have signed it
4. The declared public key's fingerprint matches the filename

This means:
- You can't forge an authorized key file
- You can't take an authorized key file from one node and use it on
  another (signer fingerprint won't match)
- The node is the sole authority for granting API access

## Trust Fold-Back

Through extends chains, permissions can only **narrow**, never expand.
A child directive's permissions must be a subset of its parent's
effective permissions. This is enforced by the
`narrow_against_parent_effective` merge strategy.

The source space matters for trust level: a system-signed binary
reached through a user-tier descriptor is capped at TrustedUser, not
TrustedSystem. A single untrusted ancestor taints the entire chain.

## Empty = Deny-All

At every enforcement layer, an empty capability set means nothing is
authorized:

- Empty `effective_caps` on a callback token → deny-all at dispatch
- Empty principal scopes → no thread launch
- `require_all(&[])` → trivially satisfied (but composing to empty
  effective_caps from an empty permissions declaration means deny-all)

## No Permissions = Safe

A directive with `permissions.execute: []` cannot invoke any tools or
services. This is the safest default for read-only or prompt-only
workflows.
