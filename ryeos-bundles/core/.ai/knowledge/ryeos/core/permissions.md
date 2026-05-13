---
category: ryeos/core
tags: [fundamentals, permissions, capabilities, security]
version: "1.0.0"
description: >
  How capabilities and permissions work in Rye OS — capability
  strings, permission narrowing, and the enforcement model.
---

# Permissions and Capabilities

Rye OS uses **capability-based access control** to gate execution.
Every tool call and service invocation is checked against the calling
context's permission set.

## Capability Strings

Capabilities are dot-namespaced strings with optional glob wildcards:

```
ryeos.execute.tool.ryeos.file-system.*
ryeos.execute.service.fetch
ryeos.execute.service.bundle/install
runtime.execute
bundle.read
net.call
```

The general pattern is:

```
ryeos.execute.<kind>.<namespace>.<name>
```

Where:
- `<kind>` is `tool`, `service`, etc.
- `<namespace>` is the item's category path
- `<name>` is the specific item name
- `*` matches any remaining segments

## How Permissions Work

### Directive Declarations
Directives declare required capabilities in frontmatter:

```yaml
permissions:
  execute:
    - ryeos.execute.tool.ryeos.file-system.*
    - ryeos.execute.service.fetch
```

An empty list `[]` means no tool execution — read-only.

### Permission Narrowing
Through extends chains, permissions can only **narrow**, never expand.
A child directive's permissions must be a subset of its parent's
effective permissions. This is enforced by the
`narrow_against_parent_effective` merge strategy.

### Runtime Enforcement
When a directive action invokes a tool or service:
1. The daemon extracts the item's `required_caps`
2. Checks each cap against the directive's `effective_caps`
3. Uses fnmatch wildcard matching
4. Denies access if any required cap is missing

## Tool and Service Requirements

Tools and services declare their own capability requirements:

```yaml
# tool: ryeos/core/fetch
required_caps: ["bundle.read"]

# service: bundle/install
required_caps: ["ryeos.execute.service.bundle/install"]
```

When the directive calls the tool, the daemon checks:
1. Does the directive have a cap matching the tool's `required_caps`?
2. If yes → execute. If no → permission denied.

## Special Capabilities

| Capability        | Who Has It            | Description                       |
|-------------------|-----------------------|-----------------------------------|
| `runtime.execute` | Runtimes (not directives) | Required by runtime binaries |
| `bundle.read`     | Tools with read access    | Read from bundle CAS         |
| `net.call`        | HTTP adapter tools        | Make outbound HTTP requests  |

## No Permissions = Safe

A directive with `permissions.execute: []` cannot invoke any tools or
services. This is the safest default for read-only or prompt-only
workflows. The `hello` directive uses this pattern.
