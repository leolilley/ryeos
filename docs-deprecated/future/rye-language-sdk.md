```yaml
id: rye-language-sdk
title: "Rye Language — SDK, Verbs, and Unified Capability Enforcement"
description: >
  DEPRECATED. The SDK surface described here is not being built. The source
  format (YAML frontmatter + markdown) IS the language — the daemon reads it
  directly. The CLI provides sign, inspect, execute, fetch, bundle operations.
  create_directive handles meta-programming. No Python/JS wrapper is needed.
  Parts of this doc HAVE been implemented: cap system unification (see
  CAP-SYSTEM-ADVANCED-COMPLETE), the Authorizer, wildcard/implication support,
  and canonical cap derivation. The SDK surface, verb registry, and type-theoretic
  framing are archived as design exploration, not implementation direction.
tags: [sdk, verbs, capabilities, language, enforcement, registry, facets, simulation, caching, effects, types, proof, deprecated]
version: "0.3.0"
status: deprecated
superseded_by:
  - CAP-SYSTEM-ADVANCED-COMPLETE
  - RYE-LANGUAGE-GAPS-IMPLEMENTATION
``]

# Rye Language — SDK, Verbs, and Unified Capability Enforcement

> **Status: DEPRECATED.** This doc is archived design exploration. The cap
> system unification described herein has been implemented (see
> CAP-SYSTEM-ADVANCED-COMPLETE). The SDK surface is not being built — the
> source format is the language, the CLI covers all operations, and
> `create_directive` handles meta-programming. See
> RYE-LANGUAGE-GAPS-IMPLEMENTATION for the active implementation plan.

---

## The Insight

RYE already has a programming language. The type system is kind schemas.
Functions are tools. Programs are directives. Modules are bundles. The
interpreter is the LLM. The runtime is the daemon. Provenance is the chain.

What's missing isn't the language — it's the surface syntax. Today you
author items by hand-writing YAML frontmatter and markdown. The SDK
generates the exact same files. The daemon doesn't know or care what
produced the data on disk.

Separately, the capability system has a structural gap: two enforcement
paths with different semantics coexist, and verbs are hardcoded strings.
A verb registry on the node makes verbs extensible and aligns enforcement
into a single path.

---

## Current State

### Items are already programs

A directive with typed inputs, typed outputs, permissions, extends
inheritance, and an LLM-interpreted body is a function. A state graph
with nodes, edges, conditional routing, and foreach fan-out is a program.
A kind schema with a `composed_value_contract` is a type definition.

The execution flow is:

1. Caller sends `rye_execute` with an item ref and input parameters
2. Daemon resolves the extends chain, validates against kind schema
3. Daemon checks caller caps against item's `required_caps`
4. Daemon spawns the appropriate runtime (directive, graph, knowledge)
5. Runtime interprets the item body
6. Results are recorded in the chain and stored in CAS

Every step is data. Every step is signed. Every step is reproducible.

### Two enforcement paths

Runtime caps (tools, directives, graphs):

- Pattern: `rye.<verb>.<kind>.<namespace>.<item>`
- Wildcard matching via `cap_matches()` — `*` and `?` suffix wildcards
- Implication rules via `expand_capabilities()` — `rye.execute.*` implies `rye.fetch.*`
- Code: `ryeos-runtime/src/capability_tokens.rs`

Service caps (daemon endpoints):

- Pattern: flat `<domain>.<action>` — `node.maintenance`, `commands.submit`
- Exact `contains()` check in `CompiledServiceInvocation` (route path)
- Exact `contains()` check in `enforce_runtime_caps()` (execute path)
- Code: `ryeosd/src/routes/invokers/service_invocation.rs`, `ryeosd/src/dispatch.rs`

The gap: `rye.execute.service.*` cannot satisfy `node.maintenance`. A
principal with broad execute access cannot invoke services. The wildcard
semantics that work for tools and directives don't apply to services.

### Verbs are hardcoded

`expand_capabilities()` in `capability_tokens.rs` hardcodes three verbs:

```rust
if cap == "rye.*" {
    to_add.push("rye.execute.*".to_string());
    to_add.push("rye.fetch.*".to_string());
    to_add.push("rye.sign.*".to_string());
}
```

Adding a new verb (e.g., `read`, `write`, `review`, `deploy`) means
modifying this function and redeploying the daemon. There's no registry,
no enum, no config. The verb is just whatever the second dot-segment
happens to be.

---

## Target State

### Part 1: The SDK

The SDK generates the exact same signed YAML/markdown files the daemon
already consumes. No new parsers. No new file formats. No new runtime.

#### Authoring

```python
import rye

directive("rye/code/quality/review",
    extends="rye/agent/core/base_review",
    model=rye.model("general"),
    limits=rye.limits(turns=10, tokens=32000),
    context=rye.context(
        before="rye/code/quality/practices",
        after="rye/code/quality/gates",
    ),
    inputs=[
        rye.input("changed_files", type="array", required=True),
        rye.input("base_ref", type="string", default="HEAD~1"),
        rye.input("severity", type="string", default="warning",
                   values=["info", "warning", "error"]),
    ],
    outputs=[
        rye.output("verdict", type="string",
                    values=["approve", "request_changes", "reject"]),
        rye.output("reasoning", type="string"),
        rye.output("findings", type="array"),
    ],
    permissions=rye.permissions(
        execute=[
            "rye/file-system/read",
            "rye/code/quality/gate",
            "rye/git/diff",
        ],
        fetch=["directive:*", "tool:*"],
    ),
    body="""
Review the changed files against quality practices.
Use severity to filter findings.
Return a verdict with reasoning.
""",
)
```

Produces `.ai/directives/rye/code/quality/review.md` — the exact same file
a human would write. Signs it with Ed25519. The daemon processes it the
same way.

#### Execution

```python
# Call a directive, capture output
changed = rye.execute("rye/git/changed_files",
    base_ref="HEAD~1",
    chain="my-review-session",
)

# Branch on result
if len(changed.files) > 20:
    rye.execute("rye/notify/slack",
        channel="#reviews",
        message=f"Large PR: {len(changed.files)} files",
    )

# Parallel fan-out — daemon handles pooling
reviews = rye.execute("rye/code/quality/review",
    changed_files=changed.files,
    severity="warning",
    chain="my-review-session",
    parallel=4,
)

# Retry — daemon policy, not SDK logic
result = rye.execute("rye/deploy/staging",
    ref="HEAD",
    chain="deploy-session",
    retries=3,
    backoff="exponential",
    on_failure="rollback",
)

# CAS-backed caching — automatic
# Same inputs produce the same content hash → cached result
result = rye.execute("rye/code/quality/review",
    changed_files=["src/parser.rs"],
    chain="my-review-session",
)
```

The SDK is thin: `rye.execute()` dispatches to the daemon. The daemon
handles pooling, retry, caching, scheduling, permission checks, chain
recording. The SDK is just control flow around results.

#### Compose at the SDK level

For cases the graph runtime can't express — like "review, auto-fix if
needed, re-review":

```python
result = rye.execute("rye/code/quality/review",
    changed_files=changed.files,
    chain="my-review-session",
)

if result.verdict == "request_changes":
    fix = rye.execute("rye/code/apply_fixes",
        findings=result.findings,
        chain="my-review-session",
    )
    result = rye.execute("rye/code/quality/review",
        changed_files=fix.modified_files,
        chain="my-review-session",
    )
```

#### Meta-programming

```python
for src in glob("src/**/*.py"):
    test_path = src.replace("src/", "tests/").replace(".py", "_test.py")

    directive(f"rye/tests/run_{test_path.replace('/', '_')}",
        model=rye.model("general"),
        limits=rye.limits(turns=3),
        permissions=rye.permissions(execute=["rye/bash/bash"]),
        inputs=[rye.input("command", type="string",
                          default=f"pytest {test_path}")],
        body=f"Run pytest on {test_path}. Report failures.",
    )
```

`create_directive` already does this at the LLM level. The SDK does it
at the programming level. Same output: a signed markdown file the daemon
executes.

#### Chain inspection and replay

```python
chain = rye.chain.get("my-review-session")

for entry in chain.entries:
    print(f"  {entry.timestamp} | {entry.directive} → {entry.duration}ms")

print(f"Total tokens: {chain.total_tokens}")

# Replay — same inputs → same CAS refs → cached
replay = rye.chain.replay("my-review-session")
```

---

### Part 2: The Verb Registry

The verb registry replaces hardcoded capability expansion. Verbs are
registered on the node, like routes.

#### Registry definition

```yaml
# Node-level verb registry (could be a config file, a signed item, or
# constructed at startup from bundle metadata)
verbs:
  execute:
    description: "Invoke tools, directives, graphs, services"
    implies: [fetch]
    match: "rye.execute.*"

  fetch:
    description: "Read items from store"
    implies: []
    match: "rye.fetch.*"

  sign:
    description: "Sign items"
    implies: [fetch]
    match: "rye.sign.*"
```

Adding a new verb is just adding an entry:

```yaml
  read:
    description: "Read-only access"
    implies: []
    match: "rye.read.*"

  write:
    description: "Mutating access"
    implies: []
    match: "rye.write.*"
```

No daemon code changes. No redeployment. The registry drives expansion,
implication, and matching.

#### `expand_capabilities()` becomes registry-driven

```rust
fn expand_capabilities(caps: &[String], verbs: &VerbRegistry) -> BTreeSet<String> {
    let mut expanded: BTreeSet<String> = caps.iter().cloned().collect();

    for cap in caps {
        // rye.* → expand to every registered verb
        if cap == "rye.*" {
            for verb in verbs.all() {
                expanded.insert(format!("rye.{verb}.*"));
            }
        }
        // rye.execute.something → add implications from verb registry
        else if let Some((verb, suffix)) = cap.strip_prefix("rye.").and_then(|s| s.split_once('.')) {
            if let Some(verb_def) = verbs.get(verb) {
                for implied in &verb_def.implies {
                    expanded.insert(format!("rye.{implied}.{suffix}"));
                }
            }
        }
    }

    expanded
}
```

`rye.*` now expands to all registered verbs, not just the three
hardcoded ones. Implication rules come from the registry, not from
if-else chains. Adding `read` to the registry means `rye.read.*` works
in every scope check, everywhere, immediately.

---

### Part 3: Unified Enforcement

The two enforcement paths converge to one.

#### Before: two paths, different semantics

**Route path** (`CompiledServiceInvocation`):
```rust
// Exact contains() — no wildcards, no expansion
for cap in &self.required_caps {
    if !scopes.contains(cap) {
        return Err(RouteDispatchError::Unauthorized);
    }
}
```

**Execute path** (`enforce_runtime_caps`):
```rust
// Exact contains() — no wildcards, no expansion
let missing: Vec<String> = required_caps
    .iter()
    .filter(|cap| !caller_scopes.contains(cap))
    .cloned()
    .collect();
```

**Runtime callback path** (directive runtime harness):
```rust
// Proper wildcard matching via cap_matches
.any(|cap| ryeos_runtime::cap_matches(cap, required))
```

Three paths. Three semantics. A cap that works in one path may not work
in another.

#### After: one path, one semantics

All three paths call `check_capability()`:

```rust
pub fn check_capability(granted_caps: &[String], required_cap: &str) -> bool {
    let expanded = expand_capabilities(granted_caps, &verb_registry);
    expanded.iter().any(|g| cap_matches(g, required_cap))
}
```

`CompiledServiceInvocation`:
```rust
if !check_capability(scopes, &self.required_caps) {
    return Err(RouteDispatchError::Unauthorized);
}
```

`enforce_runtime_caps`:
```rust
let missing: Vec<String> = required_caps
    .iter()
    .filter(|cap| !check_capability(caller_scopes, cap))
    .cloned()
    .collect();
```

One function. One matching algorithm. One expansion logic. Wildcards
work for services the same way they work for tools and directives.

---

### Part 4: Service Cap Derivation

Service caps become path-derived from the YAML location, matching the
`rye.<verb>.<kind>.<path>` pattern.

| YAML path | Current cap | Derived cap |
|---|---|---|
| `services/bundle/install.yaml` | `node.maintenance` | `rye.execute.service.bundle.install` |
| `services/bundle/remove.yaml` | `node.maintenance` | `rye.execute.service.bundle.remove` |
| `services/bundle/list.yaml` | `[]` | `rye.execute.service.bundle.list` |
| `services/commands/submit.yaml` | `commands.submit` | `rye.execute.service.commands.submit` |
| `services/threads/get.yaml` | `[]` | `rye.execute.service.threads.get` |
| `services/threads/list.yaml` | `[]` | `rye.execute.service.threads.list` |
| `services/node/sign.yaml` | `node.maintenance` | `rye.execute.service.node.sign` |
| `services/rebuild.yaml` | `node.maintenance` | `rye.execute.service.rebuild` |
| `services/system/status.yaml` | `[]` | `rye.execute.service.system.status` |

The `ServiceDescriptor.required_caps` can be auto-derived from
`service_ref` at build time:

```rust
// Instead of hand-declaring:
//   required_caps: &["node.maintenance"],
// Auto-derive from service_ref:
let cap = format!("rye.execute.service.{}", service_ref.replace("service:", ""));
```

One source of truth: the YAML path. The cap string is derived, not
declared. No divergence between route compile-time caps and execute-time
caps.

---

### Part 5: User-Defined Verbs

Verbs aren't limited to built-ins. Bundle authors and operators can
register their own verbs.

#### In the SDK

```python
@rye.verb("review", implies=["fetch"], description="Review code changes")
def review(changed_files: list[str], severity: str = "warning"):
    """Review code changes against quality practices."""
    return rye.execute("rye/code/quality/review",
        changed_files=changed_files,
        severity=severity,
    )

@review.on("request_changes")
def auto_fix(findings, changed_files):
    fix = rye.execute("rye/code/apply_fixes", findings=findings)
    return review(fix.modified_files)

@review.on("reject")
def block_merge(reasoning):
    rye.execute("rye/notify/block_merge", reason=reasoning)
```

This registers `review` in the verb registry. The daemon sees
`rye.review.*` in scopes and expands it. The implication rules (`review`
implies `fetch`) work the same as `execute` implying `fetch`. No special
casing.

#### CLI alignment

One registration, both surfaces:

```bash
# CLI
rye review --files src/parser.rs src/kind.rs --severity error
rye deploy --ref HEAD --target production

# Python — same verbs, same handlers, same chain
result = review(["src/parser.rs", "src/kind.rs"], severity="error")
deploy(target="production")
```

Verb registration writes a tool item to the bundle:

```yaml
# .ai/tools/my-org/verbs/review.yaml
---
kind: tool
verb: review
handler: my_pkg.verbs:review
implies: [fetch]
on_request_changes: my_pkg.verbs:auto_fix
on_reject: my_pkg.verbs:block_merge
---
```

The daemon sees the verb, routes to the handler. Hooks (`on_*`) are
conditional edges in the execution graph. Same model, declared inline.

#### Verb capabilities

When a user verb is registered, its capabilities follow the same
namespace:

```
rye.review.*              # grants all review verbs
rye.review.tool.*         # grants review tool calls
rye.review.directive.*    # grants review directive calls
```

A principal scoped to `rye.review.*` can execute any review verb but
can't deploy. `rye.execute.*` still grants everything. The hierarchy is
real because the enforcement is unified.

---

## Part 6: Capabilities as an Effect Type System

The capability system isn't just security — it's a type system for side
effects.

In languages with algebraic effects, a function declares what effects it
can perform. A handler decides what to do about them. The type checker
verifies that only declared effects are used. If a function tries to
perform an undeclared effect, it doesn't compile.

RYE already has this:

- **Effect declaration** = `permissions.execute` and `permissions.fetch` in
  a directive's frontmatter
- **Effect handler** = `check_capability()` at dispatch time
- **Type checker** = the daemon's enforcement path (once unified)
- **Effect scope** = `effective_caps` in the `LaunchEnvelope`
- **Effect narrowing** = child directives inherit a subset of parent caps

A directive that declares `execute: [rye/file-system/read]` is saying "I
perform the effect of reading files." The daemon checks this against the
caller's caps. The runtime enforces it at callback time. If the LLM tries
to call `rye/file-system/write`, the runtime rejects it — undeclared
effect.

This isn't metaphorical. The mapping is exact:

| Algebraic effects | RYE |
|---|---|
| `effect` keyword | `permissions.execute` list |
| Effect handler | `check_capability()` / daemon dispatch |
| Type checker | enforcement path (runtime + route) |
| Effect scope | `effective_caps` in `LaunchEnvelope` |
| Scoped effects | child inherits subset of parent caps |
| Effect polymorphism | extends chain composes permissions |

The SDK makes this visible:

```python
# The type checker runs at authoring time, not just dispatch time
directive("rye/code/quality/review",
    permissions=rye.permissions(
        execute=["rye/file-system/read"],
        fetch=["directive:*"],
    ),
    body="""
Review the code. If you need to write files, you can't — you didn't
declare that effect.
""",
)

# Author-time check: body references rye/file-system/write?
# → EffectError: body uses undeclared effect 'rye/file-system/write'
# → Add it to permissions.execute, or change the body.
```

The author-time check works because the body is text and the effect
declarations are data. The SDK can statically analyze the body for
references to items the directive hasn't declared in its permissions.
It's not perfect (the LLM might generate a reference at runtime that
wasn't in the source text), but it catches the common case — and the
runtime enforcement catches the rest.

### Effect narrowing through extends

When directive B extends directive A, the child's `effective_caps` is
the intersection of:

1. The parent's `effective_caps` (from the caller's scope)
2. The child's own `permissions` declaration

You can only narrow, never widen. This is lexical scoping for effects.

```python
# Parent: broad access
directive("rye/agent/core/base_review",
    permissions=rye.permissions(
        execute=["rye/file-system/read", "rye/file-system/write"],
        fetch=["*"],
    ),
)

# Child: narrow — can read but not write
directive("rye/code/quality/review",
    extends="rye/agent/core/base_review",
    permissions=rye.permissions(
        execute=["rye/file-system/read"],  # write removed
        fetch=["directive:*"],
    ),
)
# → effective_caps = intersection(parent, child) = [read, directive:*]
# → write is gone. The LLM literally cannot write files.
```

### Polymorphic effects via kind schemas

The `composed_value_contract` in a kind schema IS the effect signature.
Different items of the same kind can have different effect declarations,
but they all conform to the same structural contract. This is like
implementing a trait with different concrete types.

The `inventory_kinds` field on `KindSchema` determines what items a
runtime can see. A directive that declares `inventory_kinds: [tool]` can
only dispatch tools — it can't even *see* other directives or graphs in
its inventory. This is the most restrictive form of effect scoping: the
item doesn't even know the effect exists.

### What this means for the SDK

The SDK can expose effect types as Python type annotations:

```python
from rye import Effects

@rye.verb("review", effects=Effects(execute=["rye/file-system/read"]))
def review(changed_files: list[str]) -> str:
    # The type checker knows this function can only read files.
    # If the body (or any directive it calls) tries to write,
    # it's a type error at authoring time.
    ...
```

---

## Part 7: The Chain as a Proof Term

In dependent type theory, a proof is a program. You don't write a proof
separate from the program — the program IS the proof, and the type
checker verifies it.

The chain is RYE's proof term. Every `rye_execute` call produces a chain
entry that proves:

1. **Who** executed it — `principal` (fingerprint)
2. **What** was executed — `item_ref` + `resolution.root` (content hash)
3. **With what inputs** — `request.inputs` (deterministic CAS hash)
4. **Under what permissions** — `effective_caps` (from envelope)
5. **Within what limits** — `hard_limits` (turns, tokens, spend)
6. **Producing what output** — `result` + `cost` (CAS-pinned)
7. **In what context** — `resolution.ancestors` (extends chain)
8. **With what inventory** — `inventory` (tool/knowledge set)
9. **Linked to what came before** — `previous` (hash of prior entry)

A chain entry is a constructive proof that "given these inputs, this
execution happened, under these constraints, producing this output." You
can verify it by:

1. Re-hashing the inputs → do they match the recorded hash?
2. Re-checking the caps → did the principal have the required scopes?
3. Re-walking the extends chain → does the composed view match?
4. Re-hashing the output → does it match the CAS entry?
5. Re-checking the link → does `previous` hash to the prior entry?

If all checks pass, the proof is valid. No trust required beyond the
signer's key.

### Replay is proof verification

`rye.chain.replay()` is proof verification. Given a chain, you replay
every entry and verify that the same inputs produce the same CAS hashes.
If the chain is valid, the replay returns the stored results without
executing anything. If the chain has been tampered with (an entry's
output hash doesn't match CAS), the replay fails at that entry.

### The chain as a build artifact

The chain is itself a CAS-stored artifact. It has a content hash. It
can be signed. It can be bundled. It can be audited by a third party who
wasn't present during execution.

This is how you prove to someone: "I reviewed this code, with this
model, under these permissions, producing this verdict." You give them
the chain. They verify the hashes. The chain is the proof.

---

## Part 8: CAS as Referential Transparency

In functional programming, referential transparency means: calling a
function with the same arguments always produces the same result. You
can substitute the call with its return value without changing the
program's behavior.

RYE's CAS enforces this mechanically:

1. Hash the item ref + inputs + context positions → `inputs_hash`
2. Execute the directive
3. Hash the output → `output_hash`
4. Store `(inputs_hash, output_hash)` in CAS

Next time someone calls the same directive with the same inputs:

1. Compute `inputs_hash` → already in CAS
2. Return `output_hash` → no execution needed

This isn't caching. Caching has a cache invalidation problem. CAS doesn't
— the hash IS the validity check. Same content, same hash, same result.
Forever. No TTL. No staleness. No "did the underlying data change?"

### CAS makes the language pure

In Haskell, purity is enforced by the type system — you can't perform
IO inside a pure function. In RYE, purity is enforced by content
addressing — if the inputs hash matches, the output MUST be the same.

The stochastic nature of LLM execution breaks strict referential
transparency (calling the same directive twice might produce different
text). But the CAS hash of the *stored output* is stable. The chain
records which specific output was produced at which specific invocation.
Replay returns that exact output.

So RYE has two modes:

- **Strict mode** (deterministic tools, graphs): same inputs → same
  output, always. CAS-enforced.
- **Stochastic mode** (directives, knowledge): same inputs → possibly
  different output, but each specific output is CAS-pinned and
  replayable.

The SDK exposes this distinction:

```python
# Strict — always returns cached result
result = rye.execute("rye/git/diff", base_ref="HEAD~1")
# → CAS hit. Instant. Deterministic.

# Stochastic — may produce new output, or return cached if policy allows
result = rye.execute("rye/code/quality/review",
    changed_files=["src/parser.rs"],
    cache="allow",  # use cached if available, but don't require
)
# → May execute. May return cached. Policy-controlled.

# Force re-execution even if cached
result = rye.execute("rye/code/quality/review",
    changed_files=["src/parser.rs"],
    cache="never",
)
# → Always executes. New chain entry. New CAS hash.
```

### CAS refs as linear values

A CAS hash is an immutable reference. You can't modify what it points to.
You can only create new hashes. This is linearity — values that can be
consumed but not mutated.

```python
# This ref is immutable
ref = rye.cas.store(data=json.dumps(findings))
# ref = "sha256:a1b2c3..."

# You can derive new refs, but can't change old ones
annotated = rye.cas.store(data=json.dumps({
    "findings_ref": ref,
    "timestamp": "2026-05-04",
}))
# New ref. Old ref unchanged. Both valid forever.
```

This makes pipelines composable. Each step takes refs as inputs and
produces refs as outputs. No shared mutable state. No race conditions.
The dependency graph is explicit in the refs.

---

## Part 9: Kind Schemas as Type Classes

A kind schema defines an interface that items must implement. This is a
type class.

The `composed_value_contract` is the class constraint. Every item of
kind `directive` must satisfy the directive contract: it must have a
`body` (string), it may have `extends`, `permissions`, and `context`.
If it doesn't satisfy the contract, the parser rejects it at load time.

The `extends` field is like inheritance — a directive can extend another
directive, inheriting its permissions and context. But kind schemas
could also support *implementation* — multiple items that conform to the
same interface without sharing an inheritance hierarchy.

### Implementation polymorphism

Imagine a kind schema for "reviewer":

```yaml
kind: reviewer
composed_value_contract:
  root_type: mapping
  required:
    review:
      type: single
      prim: string
    verdict:
      type: single
      prim: string
  optional:
    severity:
      type: single
      prim: string
```

Multiple items can implement this interface:

```yaml
# tools/reviewers/security.yaml
---
kind: reviewer
review: "Check for security vulnerabilities"
verdict: "pass/fail"
severity: "critical"
---

# tools/reviewers/performance.yaml
---
kind: reviewer
review: "Check for performance issues"
verdict: "pass/warn/fail"
severity: "warning"
---
```

A directive can be polymorphic over reviewers:

```python
# The SDK knows "reviewer" is a type class
# It can dispatch to any item that implements it
result = rye.execute("reviewer:security", files=changed.files)
result = rye.execute("reviewer:performance", files=changed.files)

# Or dispatch to all implementers
results = rye.execute("reviewer:*", files=changed.files)
# → runs every item of kind "reviewer"
```

This already works mechanically — `inventory_kinds` can list any kind,
and the daemon inventories all items of that kind. What's missing is the
type-class semantics: the SDK understanding that `reviewer:*` means
"any item satisfying the reviewer contract."

### The composer as a type class method

The `composer` field on `KindSchema` is like a type class method. The
directive kind uses `handler:rye/core/extends-chain` — this is the
method that composes the extends chain. A different kind could use a
different composer — `handler:rye/core/identity` for kinds that don't
compose, or a custom handler for kinds with unique composition rules.

This is already how it works. The insight is that it's type-class
dispatch: the kind selects the composer, the composer implements the
composition logic, and the `composed_value_contract` is the constraint
that all implementations must satisfy.

---

## Part 10: Behavioral Types for LLM Programs

The body of a directive is a program the LLM interprets. But it's not a
program in the traditional sense — it's stochastic, context-dependent,
and the LLM has significant freedom in how it interprets the
instructions.

RYE already constrains the LLM's behavior through:

1. **Turns limit** — how many LLM calls the runtime allows
2. **Token limit** — how many tokens the runtime allows
3. **Permission set** — what effects the LLM can perform
4. **Input types** — what shape the inputs must have
5. **Output types** — what shape the outputs must have
6. **Inventory** — what tools the LLM can see and call

This is a behavioral type. It doesn't specify what the LLM *will* do,
but it specifies the *space of possible behaviors*. The execution must
stay within this space.

```python
directive("rye/code/quality/review",
    model=rye.model("general"),
    limits=rye.limits(
        turns=10,       # at most 10 LLM turns
        tokens=32000,   # at most 32k tokens
        spend=0.50,     # at most $0.50
    ),
    permissions=rye.permissions(
        execute=["rye/file-system/read"],  # only reading
    ),
    inputs=[rye.input("files", type="array")],
    outputs=[rye.output("verdict", type="string")],
)
```

This is a behavioral signature: "given a list of files, within 10 turns
and $0.50, using only read access, produce a verdict string."

The chain records the actual behavior: how many turns were used, how
many tokens, what the verdict was. The behavioral type constrains the
space; the chain records the specific execution within that space.

### Probabilistic output types

LLM outputs aren't deterministic. The type system needs to account for
this. A verdict type is `approve | request_changes | reject`, but any
given execution might produce any of these values.

The kind schema handles this with value constraints:

```yaml
outputs:
  - name: verdict
    type: string
    values: [approve, request_changes, reject]
```

The runtime validates the output against this constraint. If the LLM
produces "good enough", it doesn't match — the runtime rejects it and
asks the LLM to try again (within the turns limit).

This is runtime type checking for stochastic programs. The type
constrains the space of possible outputs. The runtime enforces it. The
chain records which specific output was produced.

### Session types for multi-turn interactions

The `turns` limit creates a bounded interaction. Each turn is an LLM
call that may produce tool calls, which produce results, which feed back
into the next turn. This is a session — a structured conversation with
a specific protocol.

The protocol is defined by the directive's body (the instructions), the
inventory (the available tools), and the limits (the bounds). The session
type is: "I will give you files, you will use these tools to review them,
and within N turns produce a verdict."

The SDK could make session types explicit:

```python
@rye.session
def review_session(files: list[str]) -> rye.Output(verdict=str):
    """
    Session protocol:
    1. Receive files
    2. Read file contents (rye/file-system/read)
    3. Analyze for issues
    4. Produce verdict: approve | request_changes | reject

    Must complete within 10 turns.
    """
```

The session type is checked at authoring time (does the protocol make
sense?), enforced at runtime (are the turn/tool/cost limits respected?),
and recorded in the chain (what actually happened?).

---

## Part 11: Author-Time Type Checking

Today you discover input errors when the daemon rejects your item at parse
time. The SDK can validate against `composed_value_contract` before writing
to disk.

```python
# SDK calls the same validation the daemon uses
directive("rye/code/quality/review",
    inputs=[
        rye.input("severity", type="string", values=["low", "medium", "high"]),
        # ...
    ],
    outputs=[
        rye.output("verdict", type="integer"),  # ← kind schema expects string
        # ...
    ],
)
# → ValidationError: output 'verdict' type 'integer' not in allowed values ['string']
# → File never written. Signature never generated.
```

The validation runs against the same kind schema the daemon loads. If the
bundle doesn't have the kind schema locally, the SDK fetches it via
`rye.fetch(kind:directive)` and caches it. The feedback loop is tight:
edit, validate, fix, write, sign — all before the daemon sees anything.

For editors with LSP support, the SDK could expose a language server that
provides autocomplete for input/output types, permission strings, and
extends targets — all derived from the kind schema.

---

## Part 12: Simulation and Dry-Run

`rye.execute()` with `dry_run=True` walks the full execution path without
spawning any runtime or making any LLM calls.

```python
result = rye.execute("rye/code/quality/review",
    changed_files=["src/parser.rs"],
    severity="error",
    dry_run=True,
)
# → DryRunResult(
#     resolves=True,
#     extends_chain=["rye/agent/core/base_review", "rye/agent/core/base"],
#     context_positions=[
#         "rye/code/quality/practices",
#         "rye/code/quality/gates",
#     ],
#     required_caps=["rye/file-system/read", "rye/code/quality/gate"],
#     caller_has_caps=True,
#     estimated_tokens=3200,  # from kind schema metadata
#     cost_estimate_usd=0.008,
# )
```

The daemon already does most of this work when it processes a real
`rye_execute` — it resolves extends, validates the kind schema, checks
caps, composes context. Dry-run just stops before spawning the runtime.

This unlocks workflows like:

```python
# Validate a whole pipeline without spending tokens
pipeline = [
    rye.execute("rye/git/changed_files", base_ref="origin/main", dry_run=True),
    rye.execute("rye/code/quality/review", changed_files=["*"], dry_run=True),
    rye.execute("rye/tests/run", coverage=True, dry_run=True),
]

for step in pipeline:
    if not step.resolves:
        print(f"FAIL: {step.item_ref} — {step.error}")
    elif not step.caller_has_caps:
        print(f"CAPS: {step.item_ref} needs {step.missing_caps}")

total_cost = sum(s.cost_estimate_usd for s in pipeline)
print(f"Estimated cost: ${total_cost:.4f}")
```

---

## Part 13: Typed Chain Queries via Facets

Threads already have an extensible facet system — key-value attributes
stored in `thread_facets` with `(thread_id, key)` uniqueness. Currently
used for cost annotations:

```
cost.turns        = "10"
cost.input_tokens = "15000"
cost.output_tokens = "3200"
cost.spend        = "0.084"
cost.provider     = "openai"
```

The facet table is the natural query surface. The SDK makes it typed:

```python
# Find all rejected reviews from the last 24 hours
reviews = rye.chain.query(
    directive="rye/code/quality/review",
    where={"facets.verdict": "reject"},
    since="24h",
)

# Find expensive executions
expensive = rye.chain.query(
    where={"facets.cost.spend": ">0.50"},
    since="7d",
)

# Aggregate: total spend per directive
spend = rye.chain.aggregate(
    group_by="directive",
    sum="facets.cost.spend",
    since="30d",
)
# → {"rye/code/quality/review": 12.40, "rye/deploy/staging": 0.85}
```

The facet system is already there. The SDK adds a query layer over it.
The daemon can index facets for query performance — the `idx_facets_thread`
index already exists in the projection schema.

### Custom facets

Directives and tools can write their own facets during execution:

```python
# Inside a directive body, or via the SDK after execution:
rye.facets.set("review.verdict", "reject")
rye.facets.set("review.findings.count", "7")
rye.facets.set("review.severity", "error")
```

These become queryable the same way:

```python
rye.chain.query(
    where={"facets.review.verdict": "reject", "facets.review.severity": "error"},
)
```

Facets turn the chain from an append-only log into a queryable database.
The append-only property is preserved — facets are write-once per thread,
overwritten only on thread completion. The query layer reads, never writes.

---

## Part 14: Incremental Evaluation

If a directive call with inputs `A, B` produces output `C` stored in CAS,
and you call it again with inputs `A, B, C`, the daemon can skip
re-evaluating the parts that only depend on `A, B`.

This is memoization at the directive level, not just the call level. The
CAS makes it possible because inputs are content-addressable.

```python
# First call — full execution
result = rye.execute("rye/code/quality/review",
    changed_files=["src/parser.rs"],
    severity="error",
)
# → chain entry { inputs_hash: "h1", output_hash: "h2" }

# Second call — same files, different severity
result = rye.execute("rye/code/quality/review",
    changed_files=["src/parser.rs"],
    severity="warning",
)
# → daemon sees inputs_hash differs, but can reuse the file analysis
#   step that only depends on changed_files (not severity)
# → partial cache hit, only re-runs the severity filtering step
```

The implementation requires directives to declare which inputs affect
which steps. This is already implicit in the `context` system — context
positions inject data that doesn't change between calls with the same
base inputs. Incremental evaluation extends this to user inputs.

This is a future optimization, not a Phase 1 feature. It requires the
facet and chain query infrastructure to be in place first.

---

## Part 15: Local-First Offline Mode

The SDK caches everything locally. If the daemon is down,
`rye.execute()` checks the local CAS for a matching input hash.

```python
# Daemon is up — normal execution
result = rye.execute("rye/code/quality/review",
    changed_files=["src/parser.rs"],
    chain="my-review-session",
)

# Daemon is down — SDK checks local CAS
result = rye.execute("rye/code/quality/review",
    changed_files=["src/parser.rs"],
    chain="my-review-session",
    offline=True,  # or automatic fallback
)
# → CacheHit: returning stored result from CAS hash "h2"
# → No LLM calls. No daemon required.
```

The chain stays consistent because the hash is deterministic. Same inputs
produce the same CAS key. The SDK stores a local copy of results it has
already seen. Operations you've done before never need the daemon.

For operations you haven't done, `offline=True` raises `CacheMiss` rather
than failing silently. The SDK never serves a guess.

---

## Migration Path

### Phase 1: Unify enforcement

1. Replace `contains()` in `CompiledServiceInvocation` with
   `check_capability()`
2. Replace `contains()` in `enforce_runtime_caps()` with
   `check_capability()`
3. All three enforcement paths now use `cap_matches` + `expand_capabilities`
4. This is a pure refactor — no cap string changes, no YAML changes

### Phase 2: Migrate service cap strings

1. Update `ServiceDescriptor.required_caps` to use
   `rye.execute.service.<endpoint>` format
2. Update service YAML `required_caps` declarations
3. Re-sign all affected items
4. `rye.execute.service.*` now works as a wildcard grant for all services
5. Old-style caps (`node.maintenance`, `commands.submit`) become dead strings

### Phase 3: Verb registry

1. Extract the hardcoded verb list from `expand_capabilities()` into a
   `VerbRegistry` struct
2. Load built-in verbs at daemon startup
3. `expand_capabilities()` takes `&VerbRegistry` instead of hardcoding
4. Add `VerbRegistry::register()` for user verbs
5. User verbs can be registered via bundle items or operator config

### Phase 4: SDK core

1. Python (or JS) library that generates signed item files
2. `rye.execute()` dispatches to daemon via HTTP/WebSocket
3. `rye.chain.*` for chain inspection and replay
4. `rye.cas.*` for content-addressable storage
5. `@rye.verb()` decorator for user verb registration
6. CLI integration: `rye <verb>` routes to registered verb handler
7. Author-time validation against `composed_value_contract`

### Phase 5: Path-derived caps

1. `ServiceDescriptor.required_caps` auto-derived from `service_ref`
2. Remove hand-declared caps from service handler Rust code
3. Derivation: `services/foo/bar.yaml` → `rye.execute.service.foo.bar`
4. One source of truth: the file path

### Phase 6: Simulation and query layer

1. `dry_run=True` flag — daemon validates and estimates without executing
2. Typed chain queries over `thread_facets` — `rye.chain.query()`
3. Custom facet writing from directives — `rye.facets.set()`
4. Facet aggregation — `rye.chain.aggregate()` with group-by and sum
5. Cost tracking dashboards powered by facet queries

### Phase 7: Offline and incremental

1. Local CAS cache in the SDK — `offline=True` mode
2. Cache key: deterministic hash of item ref + inputs + context positions
3. Incremental evaluation — directive-level memoization over CAS
4. Input dependency declaration in directives for partial cache hits

---

## What This Enables

### Capabilities are effect types

The permission system IS a type system for side effects. Declaring
`execute: [rye/file-system/read]` is declaring an effect. The daemon is
the effect handler. `check_capability()` is the type checker. Child
directives narrow their parent's effects — lexical scoping for
capabilities. The SDK exposes this as type annotations.

### The chain is a proof term

Every execution produces a constructive proof: "given these inputs, this
happened, under these constraints, producing this output." The proof is
verifiable by re-hashing. Replay is proof verification. The chain is an
auditable artifact that a third party can verify without being present
during execution.

### CAS enforces referential transparency

Same inputs → same hash → same output. Not caching — CAS. No TTL, no
staleness, no invalidation. Stochastic directives produce CAS-pinned
outputs that are replayable even though the underlying LLM call isn't
deterministic. CAS refs are linear — immutable, composable, dependency-
explicit.

### Kind schemas are type classes

`composed_value_contract` is a class constraint. Items implement the
class. The composer is the method. `extends` is inheritance. The SDK can
dispatch to any item satisfying the contract — `reviewer:*` means "any
item that implements the reviewer type class."

### Behavioral types for LLM programs

Turns, tokens, spend, permissions, inventory, input/output constraints —
these define the space of possible LLM behaviors. The runtime enforces
the bounds. The chain records the actual execution. This is runtime type
checking for stochastic programs.

### Namespace wildcards that actually work

```
rye.execute.*                    # all tools, directives, graphs, services
rye.execute.service.*            # all services
rye.execute.service.bundle.*     # all bundle operations
rye.execute.service.threads.*    # all thread operations
```

Today this doesn't work for services. After migration, it does.

### Implication rules that extend

```
rye.execute.*  →  rye.fetch.*   # existing
rye.sign.*     →  rye.fetch.*   # existing
rye.write.*    →  rye.read.*    # new, registered in verb registry
rye.deploy.*   →  rye.read.*    # new, user-defined implication
```

### One enforcement path

A cap that passes `check_capability()` passes everywhere: routes,
execute dispatch, runtime callbacks. No more "works for tools, fails for
services."

### Extensible verbs

Adding `read`, `write`, `review`, `deploy` is a registry entry, not a
code change. Bundle authors define verbs. Operators configure them.
The daemon expands and matches them automatically.

### Language without a language

The SDK doesn't introduce a new runtime. It generates the same signed
files the daemon already processes. The "language" is the data model.
The SDK is just syntax over that model.

### Author-time validation

Write-time type checking against `composed_value_contract`. Errors caught
before the file hits disk, before the daemon sees it, before any tokens
are spent. The kind schema is both the runtime validator and the editor's
type checker.

### Zero-cost simulation

Dry-run walks the full resolution path — extends chain, cap checks,
context composition, cost estimation — without spawning runtimes or
calling LLMs. Pipeline authors validate entire workflows before spending
a cent.

### Queryable execution history

Facets turn the append-only chain into a queryable database. Cost
tracking, verdict history, performance regression detection — all SQL
over the existing `thread_facets` table. Custom facets from directives
extend the query surface without schema changes.

### Deterministic offline execution

Local CAS cache means operations you've done before never need the
daemon. The content hash is the cache key. No heuristic, no staleness —
either the exact result exists or it doesn't.

### Incremental re-evaluation

When some inputs change and others don't, the daemon reuses the cached
steps that depend only on the unchanged inputs. The CAS makes partial
cache hits possible at the directive level, not just the call level.

---

## Relationship to Existing Docs

| Doc | Relationship |
|---|---|
| `bundle-build-system.md` | The build system produces signed artifacts. The SDK authors the build manifests. Verb registry controls who can trigger builds. |
| `execution-graph-scheduling.md` | The implicit execution graph from `rye_execute` calls. The SDK makes that graph explicit in control flow. |
| `natural-language-cli.md` | CLI verb registration aligns with NL intent parsing — `rye review` maps to the same handler as the Python `review()` call. |
| `node-sandboxed-execution.md` | Execution verbs like `execute` and `write` can require sandbox profiles. The verb registry carries these requirements. |
| Cap alignment notes (`.tmp/CAP-ALIGNMENT-NOTES.md`) | This doc is the implementation plan for the alignment described there. |
| `GC-ADVANCED-PATH.md` | Local CAS caching and incremental evaluation produce more intermediate artifacts. GC becomes more important for SDK users. |
| Knowledge runtime docs | Facets on knowledge threads enable queryable retrieval — "find all knowledge entries about topic X generated in the last week." |

---

## What "Everything Is Data" Means For the Language

The verb registry is data. The kind schemas are data. The items are data.
The chain is data. The SDK generates data. The daemon consumes data. The
language IS the data.

There is no compiler. There is no interpreter (except the LLM, which is
already there). There is no new runtime. The SDK writes signed YAML to
disk. The daemon reads it. The chain records what happened. The CAS makes
it reproducible.

A `@rye.verb()` decorator writes a signed tool item. A `rye.execute()`
call sends JSON to the daemon. A `rye.chain.replay()` walks stored
results. The programming model is the API surface over the existing
data model.

The language is not a thing that gets built. It is the thing that already
exists, given a surface.
