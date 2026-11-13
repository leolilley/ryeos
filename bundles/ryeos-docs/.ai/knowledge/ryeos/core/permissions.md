<!-- ryeos:signed:2026-11-13T04:43:16Z:27889cbbfd51b459198449207b626d49da1026b390136499f32e4159b1b644ed:PIYwnhVtWo5guXnyJJtTmgrGiMappLKqlgpAS6Y4OpE8e7/O6ZdmQyF4g7hGpXXpWkHaGHiBtZ46emDDY7lRDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

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
| `subject` | Which specific resource | For item-execution caps: the canonical bare item id, preserving `/` (e.g. `bundle/install`, `ryeos/file-system/read`). May contain `/`, `.`, `_`, `-`, `0-9`, `a-z`. |

The rule: `cap = ryeos.<verb>.<kind>.<bare_id>`, where `<bare_id>` is
exactly the path after `kind:` in the canonical ref. The `.` characters
before the subject separate the RyeOS capability envelope; `/` inside
the subject preserves item namespace and filesystem layout. A `.` inside
a subject is literal in wildcard matching — a grant of `scheduler.*`
does NOT match `scheduler/register`; grant `scheduler/*` instead.
Dot-form names survive only as internal handler endpoint keys
(e.g. `endpoint: scheduler.register`), never as capability subjects.

Examples:

```
service:scheduler/register   →  ryeos.execute.service.scheduler/register
service:objects/closure/get  →  ryeos.execute.service.objects/closure/get
tool:ryeos/file-system/read  →  ryeos.execute.tool.ryeos/file-system/read
directive:my/workflow        →  ryeos.sign.directive.my/workflow
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

- `require("ryeos.execute.service.bundle/install")` — single-clause
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
| `ryeos.execute.service.*` | `ryeos.execute.service.bundle/install` |
| `ryeos.execute.*` | `ryeos.execute.service.bundle/install` |
| `ryeos.*` | Any `ryeos.*` cap |
| `*` | Everything (superuser) |

### Required-Side Wildcards

A broad requirement is satisfied by any narrow grant:

| Required | Satisfied By |
|---|---|
| `ryeos.execute.service.*` | `ryeos.execute.service.bundle/install` |
| `ryeos.execute.*` | `ryeos.execute.service.bundle/install` |
| `ryeos.*` | Any `ryeos.*` grant |

### Path-Prefix Wildcards

A prefix wildcard matches at the `/` boundary of the subject's bare id:

- `ryeos.execute.service.bundle/*` matches `bundle/install` ✓
- `ryeos.execute.service.bundle/*` matches `bundleX` ✗ (`/` is literal)
- `ryeos.execute.service.bundle.*` matches `bundle/install` ✗ (`.` is
  literal — dot-form grants satisfy nothing)

The `*` becomes `.*` in regex; every other character, including `.` and
`/`, is escaped, so prefix wildcards don't leak across segments.

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

The engine composition pipeline uses each kind's signed generic composer rules
to lift `requires.capabilities.declared` into `effective_caps` — the composed
capability set for the launched item. Across an `extends` chain, every direct
child may only narrow the already-narrowed parent; authority discarded by an
intermediate document cannot be recovered by the root.
This is what the item is allowed to dispatch back to the daemon.

#### Root admission: callers lend authority

Items declare what they need; they never grant themselves authority. At root
admission every cap in the composed `declared` set (an `extends` chain
included) must be covered by the launching caller's scopes, with wildcards in
the caller's grant working as everywhere else. The thread then runs with
exactly its declared caps plus the runtime caps its own signed bundle manifest
grants — never the caller's full scopes. Manifest runtime caps need no caller
coverage: a bundle's signed manifest gives its runtime that authority.

A caller that does not cover every declared cap is refused (403) with the
typed code `caller_capability_not_covered`, which names the item, the caller,
every uncovered cap and the cure: grant the caller the cap, or narrow the
item's `requires.capabilities.declared`. The same rule applies to a scheduled
fire (the schedule's stored caps are its scopes) and to an operator follow-up
on a running thread: the follow-up runs with the thread's sealed caps exactly,
and the sender must cover them. Routes whose principal carries no scopes
cannot launch an item that declares caps.

The thread records the covering principal and, when the caller authenticated
under an authorized-key grant, that grant's digest, so work running under a
revoked grant can be found.

### 4. Effective Caps → Callback Tokens

When the daemon mints a callback token for the subprocess, the
`effective_caps` are copied onto the token. When the child process
calls back, the daemon enforces these caps before dispatch. The child
cannot escalate beyond what the parent allowed.

Hook callbacks are narrower. Authored hooks use the launching item's admitted
caps. Configured hook layers use only the dispatch grants declared by their own
signed config source. Callback preflight selects the exact captured hook before
capability evaluation, so an operator or project hook cannot borrow a
privileged bundle program's authority.

#### Tools and child launches

An item's effective caps are its composed declared caps plus the runtime
caps its own signed manifest grants (`requires.capabilities.manifest`).

- **Tools are leaves.** A tool has no surface to honour self-declared action
  authority. A tool that carries `requires.capabilities.declared`, even an
  empty list, is refused before launch. Only manifest runtime authority is
  honoured for tools.
- **Dispatching a child.** A callback that dispatches an item requires the
  caller's effective caps to cover that item's execute capability
  (`ryeos.execute.<kind>.<id>`).
- **Inline children** launch under the ordinary admission policy: their
  effective caps are their own composed caps.
- **Detached follow children** are bounded by the parent. The parent must
  hold the child's execute capability, each cap the child declares must be
  covered by the parent's caps (and is kept at the child's exact shape), and
  the child keeps the runtime caps its own manifest grants.

A process that needs to drive RyeOS from inside a workload, such as an
external agent in a hosted worker, uses the admitted workload client (see
`knowledge:ryeos/core/execution/worker-hosted-execution`). Its grant is the
intersection of the project's per-item ceilings, the caller's scopes, the
root delegation ceiling and node policy, over a finite list of items, ref
bindings and call forms. There is no separate authority class for tools.

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

Executable `requires.capabilities` authority can only **narrow**, never expand,
through graph and directive extends chains. Every deepest-first inheritance
edge is validated independently by `narrow_requires_capabilities`, so a root
cannot recover authority discarded by an intermediate ancestor. Other kinds
can declare the generic `narrow_against_parent_effective` strategy for their
own mapping-shaped capability surfaces.

The source space matters for trust level: a bundle-signed binary
reached through a project-tier descriptor is capped at TrustedProject,
not TrustedBundle. A single untrusted ancestor taints the entire chain.

## Empty = Deny-All

At every enforcement layer, an empty capability set means nothing is
authorized:

- Empty `effective_caps` on a callback token → deny-all at dispatch
- Empty principal scopes → no thread launch
- `require_all(&[])` → trivially satisfied (but an empty composed
  `requires.capabilities.declared` set still means deny-all)

## No Declared Caps = Safe

A directive with `requires.capabilities.declared: []` (or no `requires` block)
cannot invoke any tools or services. This is the safest default for read-only or
prompt-only workflows.
