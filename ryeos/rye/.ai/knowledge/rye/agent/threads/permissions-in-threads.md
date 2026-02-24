<!-- rye:signed:2026-02-24T05:50:18Z:58eb480536a60d7d9f7941a9b2ce711008e0dae31e6523995655f5dcd7f36dd5:0oGepw3B_84eBZPcNcLxsRmZeopNkUNyfxzPxcSVnCtwiItw_10vtpwTiy39aBkoASMy6FPnypC6NDD-kxkMDA==:9fbfabe975fa5a7f -->
```yaml
name: permissions-in-threads
title: Permissions in Threads
entry_type: reference
category: rye/agent/threads
version: "1.1.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - permissions
  - capabilities
  - threads
  - security
  - risk-classification
  - acknowledge
references:
  - limits-and-safety
  - capability-strings
  - directive-extends
  - "docs/orchestration/permissions-and-capabilities.md"
```

# Permissions in Threads

Capability tokens control what each thread can access. Declared in directive XML, enforced by `SafetyHarness`, attenuated down the hierarchy.

## Capability String Format

```
rye.<primary>.<item_type>.<item_id_dotted>
```

| Component        | Values                                     |
|------------------|--------------------------------------------|
| `primary`        | `execute`, `search`, `load`, `sign`        |
| `item_type`      | `tool`, `directive`, `knowledge`           |
| `item_id_dotted` | Item ID with `/` → `.`, fnmatch wildcards  |

### Examples

| Capability String                                    | Allows                                    |
|------------------------------------------------------|-------------------------------------------|
| `rye.execute.tool.rye.file-system.*`                 | Execute any tool under `rye/file-system/` |
| `rye.execute.tool.rye.agent.threads.thread_directive`| Execute thread_directive (internal, used by `execute directive`) |
| `rye.execute.directive.domain.*`                     | Spawn threads for any directive under `domain/` |
| `rye.search.directive.*`                             | Search all directives                     |
| `rye.load.knowledge.agency-kiwi.*`                   | Load any knowledge under `agency-kiwi/`   |
| `rye.sign.directive.*`                               | Sign any directive                        |

## Declaring Permissions in Directives

```xml
<permissions>
  <execute>
    <tool>rye.agent.threads.thread_directive</tool>  <!-- internal capability needed by execute directive -->
    <tool>rye.agent.threads.orchestrator</tool>
    <directive>agency-kiwi.*</directive>  <!-- allows spawning threads for these directives -->
  </execute>
  <search>
    <directive>agency-kiwi.*</directive>
    <knowledge>agency-kiwi.*</knowledge>
  </search>
  <load>
    <knowledge>agency-kiwi.*</knowledge>
  </load>
</permissions>
```

> **Note:** Users call `execute directive` to spawn threads. This internally requires the `<tool>rye.agent.threads.thread_directive</tool>` capability, so it must still be declared in permissions.

### XML → Capability String Mapping

| XML Declaration                                         | Capability String                                     |
|---------------------------------------------------------|-------------------------------------------------------|
| `<execute><tool>rye.file-system.*</tool></execute>`     | `rye.execute.tool.rye.file-system.*`                  |
| `<search><directive>*</directive></search>`             | `rye.search.directive.*`                              |
| `<load><knowledge>agency-kiwi.*</knowledge></load>`    | `rye.load.knowledge.agency-kiwi.*`                    |

### Wildcard Shortcuts

```xml
<permissions>*</permissions>     <!-- God mode — all permissions -->
<execute>*</execute>             <!-- Execute everything -->
<search>*</search>               <!-- Search everything -->
```

## Fail-Closed Default

**If no capabilities are declared, ALL actions are denied.**

```python
if not self._capabilities:
    return {"error": f"Permission denied: no capabilities declared. "
                     f"Cannot {primary} {item_type} '{target}'"}
```

A directive with empty/missing `<permissions>` can't execute tools, load knowledge, or search anything. Prevents accidental privilege escalation.

## Permission Checking Flow

The runner checks before every tool call dispatch:

```python
inner_primary = tc_name.replace("rye_", "", 1)   # "rye_execute" → "execute"
inner_item_type = tc_input.get("item_type", "tool")
inner_item_id = tc_input.get("item_id", "")

denied = harness.check_permission(inner_primary, inner_item_type, inner_item_id)
if denied:
    # Error returned to LLM as tool result — LLM sees the denial
    messages.append({"role": "tool", "tool_call_id": ..., "content": str(denied)})
    continue  # skip execution
```

### Check Algorithm

```python
def check_permission(self, primary, item_type, item_id=""):
    # Internal thread tools always allowed
    if item_id and item_id.startswith("rye/agent/threads/internal/"):
        return None

    # No capabilities = all denied
    if not self._capabilities:
        return {"error": "Permission denied: no capabilities declared..."}

    # Build required capability string
    if item_id:
        item_id_dotted = item_id.replace("/", ".")
        required = f"rye.{primary}.{item_type}.{item_id_dotted}"
    else:
        required = f"rye.{primary}.{item_type}"

    # fnmatch against all capabilities
    for cap in self._capabilities:
        if fnmatch.fnmatch(required, cap):
            return None  # allowed

    return {"error": f"Permission denied: '{required}' not covered..."}
```

Key details:
- **fnmatch wildcards** — `*` matches anything, `?` matches single character
- **`/` → `.` conversion** — item IDs converted for matching
- **Internal tools always allowed** — `rye/agent/threads/internal/*` bypasses checks

## Capability Attenuation (Inheritance)

Capabilities flow down the hierarchy with attenuation — children get same or fewer capabilities, never more.

### Derivation Rules

```python
# SafetyHarness.__init__
child_caps = []
if permissions:
    child_caps = [p["content"].replace("/", ".") for p in permissions if p.get("tag") == "cap"]

if child_caps:
    self._capabilities = child_caps          # Use directive's permissions
elif parent_capabilities:
    self._capabilities = [c.replace("/", ".") for c in parent_capabilities]  # Inherit parent's
else:
    self._capabilities = []                  # Fail-closed
```

| Directive Permissions | Parent Capabilities | Result                        |
|----------------------|---------------------|-------------------------------|
| Declared             | Any                 | Uses directive's permissions  |
| Not declared         | Inherited from parent| Uses parent's capabilities   |
| Not declared         | None (root thread)  | Empty → all denied            |

### Attenuation Example

```
Root orchestrator:
  ✓ thread_directive (internal), orchestrator, directive agency-kiwi.*, search agency-kiwi.*, load agency-kiwi.*
  (spawns threads via: execute directive)

  └── qualify_leads (declares own permissions):
      ✓ thread_directive (internal), directive agency-kiwi.*, load agency-kiwi.*
      ✗ orchestrator (dropped), search (dropped)

      └── score_lead (declares own permissions):
          ✓ analysis.score_ghl_opportunity
          ✗ thread_directive (dropped), directive (dropped), knowledge loading (dropped)

      └── leaf_without_permissions (no <permissions> block):
          ✓ Inherits qualify_leads: thread_directive, directive agency-kiwi.*, load agency-kiwi.*
```

## Design Principles

1. **Start with execution leaves** — each needs exactly the tools it calls
2. **Sub-orchestrators** need `thread_directive` (internal) + `directive` patterns for children + domain knowledge loading
3. **Root orchestrators** need `thread_directive` (internal), `orchestrator` (wait/aggregate), `directive` patterns, search/load
4. **Never use `<permissions>*</permissions>` in production** — defeats the purpose
5. **LLM gets clear error** — denied permissions produce explicit messages for debugging

## Capability Risk Classification

Every capability string is assigned a **risk level** that determines how it's handled at runtime. Risk classifications are defined in `capability_risk.yaml`.

### Risk Levels

| Risk Level     | Description                                              | Example Capabilities               |
|----------------|----------------------------------------------------------|--------------------------------------|
| `safe`         | Read-only, no side effects                               | `rye.search.*`, `rye.load.*`        |
| `write`        | Modifies state but within normal operation               | `rye.execute.tool.rye.file-system.*`|
| `elevated`     | High-impact actions requiring explicit opt-in            | `rye.sign.*`, `rye.execute.directive.*` |
| `unrestricted` | Full access, no guardrails                               | `rye.*`                              |

### Policies

Each risk level maps to a **policy** that controls enforcement:

| Policy                 | Behavior                                                    |
|------------------------|-------------------------------------------------------------|
| `allow`                | Permitted without additional checks                         |
| `acknowledge_required` | Directive must include `<acknowledge>` tag to opt in        |
| `block`                | Denied unconditionally                                      |

### `capability_risk.yaml` Configuration

Risk classifications live in `capability_risk.yaml`. Custom classifications can be added for project-specific capabilities:

```yaml
risk_levels:
  safe:
    policy: allow
    patterns:
      - "rye.search.*"
      - "rye.load.*"
  write:
    policy: allow
    patterns:
      - "rye.execute.tool.rye.file-system.*"
  elevated:
    policy: acknowledge_required
    patterns:
      - "rye.sign.*"
      - "rye.execute.directive.*"
  unrestricted:
    policy: block
    patterns:
      - "rye.*"
```

## The `<acknowledge>` Tag

Directives that need elevated capabilities must explicitly opt in using the `<acknowledge>` tag inside `<permissions>`:

```xml
<permissions>
  <acknowledge>elevated</acknowledge>
  <execute>
    <directive>*</directive>
  </execute>
  <sign>
    <directive>*</directive>
  </sign>
</permissions>
```

Without the `<acknowledge>` tag, capabilities classified as `acknowledge_required` are denied even if the capability string matches.

## Most-Specific-First Matching

When a capability string matches multiple risk patterns, the **most specific** pattern wins. Specificity is determined by the number of segments in the pattern.

```
rye.search.directive.*     → matches "safe" (4 segments)
rye.*                      → matches "unrestricted" (2 segments)

Request: rye.search.directive.my-project.workflows
→ "rye.search.directive.*" is more specific → risk = safe
```

This ensures that granting broad patterns like `rye.*` doesn't accidentally classify fine-grained safe operations as unrestricted.

## Broad Capability Warnings

When broad capability patterns are granted, warnings are logged at thread startup:

| Pattern             | Warning                                                   |
|---------------------|-----------------------------------------------------------|
| `rye.*`             | "Broad capability granted: rye.* covers all operations"   |
| `rye.execute.*`     | "Broad execute capability: rye.execute.* covers all tool/directive execution" |

These warnings appear in thread logs and help identify directives with overly permissive capability grants during development and review.
