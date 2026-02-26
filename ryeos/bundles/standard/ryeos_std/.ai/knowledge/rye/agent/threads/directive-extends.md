<!-- rye:signed:2026-02-26T03:49:32Z:99bc0af98957591bbc9b4210b9d6faa83aa512745a72bb75c19897f61fa06d69:S_Tvj9EeYp1FhcZClYlIQsIZ6P0mlfTbGAF7IuQDvOTpVpZqErL_RtfDdY0hgROqSEOH5Y0gSPvYVHxwpN4kDA==:9fbfabe975fa5a7f -->
<!-- rye:unsigned -->

```yaml
name: directive-extends
title: Directive Extends Chain
entry_type: reference
category: rye/agent/threads
version: "1.0.0"
author: rye-os
created_at: 2026-02-24T00:00:00Z
tags:
  - directives
  - extends
  - inheritance
  - context
  - composition
references:
  - prompt-rendering
  - permissions-in-threads
  - thread-lifecycle
  - "docs/authoring/directives.md"
```

# Directive Extends Chain

How directives inherit context, capabilities, and behavior through the `extends` attribute.

## The `extends` Attribute

A directive can extend another directive using the `extends` attribute on the root `<directive>` tag:

```xml
<directive name="my-project/task-runner" extends="rye/agent/core/coder">
  <!-- This directive inherits from the coder base -->
</directive>
```

The value is a directive ID (slash-separated path, no extension) resolved through the standard three-tier space resolution (project → user → system).

## Chain Resolution

Extends chains are resolved **leaf → parent → root**, walking up through each `extends` reference until a directive without `extends` is reached.

```
my-project/deploy-task
  extends → rye/agent/core/coder
    extends → rye/agent/core/base
      (no extends — this is the root)
```

### Circular Detection

If a cycle is detected during resolution (e.g., A extends B, B extends A), the chain is rejected with an error. The resolver tracks visited directive IDs and raises an error if a previously seen ID is encountered.

## Context Composition

When a directive has an extends chain, `<context>` metadata from all directives in the chain is composed **root-first**. This means the base directive's context appears first, with each descendant layering on top.

### Composition Order

```
root directive context     (rye/agent/core/base)
  → parent directive context (rye/agent/core/coder)
    → leaf directive context   (my-project/deploy-task)
```

### Position Handling

Each context position (`<system>`, `<before>`, `<after>`) is composed independently:

- **`<system>`** — All system content is concatenated root-first and appended to the system message
- **`<before>`** — All before content is concatenated root-first and prepended before the directive body
- **`<after>`** — All after content is concatenated root-first and appended after the directive body

### Deduplication

Knowledge items referenced in `<context>` are deduplicated across the chain. If the same knowledge ID appears in both the base and leaf directive, it is loaded and injected only once (first occurrence wins).

## Capability Inheritance

Capabilities follow a **nearest-parent** inheritance model:

1. If the leaf directive declares its own `<permissions>`, those are used (standard attenuation rules apply)
2. If the leaf has **no** `<permissions>` block, capabilities are inherited from the nearest parent in the extends chain that declares permissions
3. If no directive in the chain declares permissions, the thread gets an empty capability set (fail-closed)

This is distinct from the parent-thread capability attenuation described in `permissions-in-threads`. The extends chain determines the *declared* capabilities; the thread hierarchy then attenuates those at runtime.

## The `<context>` Metadata Section

The `<context>` block lives inside the directive's XML fence and declares static content to inject at specific positions:

```xml
<metadata>
  <context>
    <system>You are a senior engineer specializing in deployment automation.</system>
    <before>
      <knowledge>rye/core/capability-strings</knowledge>
      <knowledge>project/deploy-conventions</knowledge>
    </before>
    <after>Always verify deployment succeeded before returning results.</after>
  </context>
</metadata>
```

### Positions

| Position   | Injection Point                                    |
|------------|---------------------------------------------------|
| `<system>` | Appended to the system message (before the loop)  |
| `<before>` | Prepended before the directive body               |
| `<after>`  | Appended after the directive body                 |

### Content Types

Context blocks support two content types:

- **Inline text** — literal strings injected as-is
- **Knowledge references** — `<knowledge>item-id</knowledge>` tags that resolve and inject knowledge item content

## Relationship to Hooks

`<context>` and hooks serve different purposes:

| Aspect      | `<context>`                        | Hooks                                 |
|-------------|-------------------------------------|---------------------------------------|
| Nature      | Static, declarative                 | Dynamic, conditional                  |
| When        | Resolved at directive parse time    | Fired at specific lifecycle events    |
| Content     | Fixed text and knowledge references | Computed at runtime via hook functions |
| Composable  | Inherited through extends chain     | Not inherited — declared per-directive|

Use `<context>` for content that is always needed (persona, knowledge, instructions). Use hooks for content that depends on runtime state (e.g., loading different knowledge based on input parameters).

## Example Chain

### Base: `rye/agent/core/base`

```xml
<directive name="rye/agent/core/base">
  <metadata>
    <context>
      <system>You are a Rye OS agent. Follow all safety and permission constraints.</system>
    </context>
  </metadata>
  <permissions>
    <search><directive>*</directive></search>
    <search><knowledge>*</knowledge></search>
    <load><knowledge>*</knowledge></load>
  </permissions>
</directive>
```

### Parent: `rye/agent/core/coder`

```xml
<directive name="rye/agent/core/coder" extends="rye/agent/core/base">
  <metadata>
    <context>
      <system>You write clean, tested code following project conventions.</system>
      <before>
        <knowledge>project/coding-standards</knowledge>
      </before>
    </context>
  </metadata>
  <permissions>
    <execute>
      <tool>rye.file-system.*</tool>
      <tool>rye.bash.bash</tool>
    </execute>
    <search><directive>*</directive></search>
    <load><knowledge>*</knowledge></load>
  </permissions>
</directive>
```

### Leaf: `my-project/deploy-task`

```xml
<directive name="my-project/deploy-task" extends="rye/agent/core/coder">
  <metadata>
    <context>
      <before>
        <knowledge>my-project/deploy-runbook</knowledge>
      </before>
      <after>Verify deployment health before returning.</after>
    </context>
  </metadata>
  <!-- No <permissions> — inherits from coder -->
</directive>
```

### Resulting Composition

**System message:**
```
You are a Rye OS agent. Follow all safety and permission constraints.
You write clean, tested code following project conventions.
```

**Before body:**
```
[content of project/coding-standards]
[content of my-project/deploy-runbook]
```

**After body:**
```
Verify deployment health before returning.
```

**Capabilities:** Inherited from `rye/agent/core/coder` (file-system, bash, search, load).
