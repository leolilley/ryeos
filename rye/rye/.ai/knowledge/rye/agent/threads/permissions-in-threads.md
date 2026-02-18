<!-- rye:signed:2026-02-18T07:19:51Z:2c3c3959e974ae4bbdd37d350513d098e5f9cd48b81c975f6ee5f875fd65b281:7fj77RrudJuMvD5z-4ggKiwO7T3MavO5le2-rj3iEGx8PiyBPtWvJtWzkpRKeUim590rs9wsfaDf88xgbSj9Dg==:440443d0858f0199 -->

```yaml
id: permissions-in-threads
title: Permissions in Threads
entry_type: reference
category: rye/agent/threads
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - permissions
  - capabilities
  - threads
  - security
references:
  - limits-and-safety
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
| `rye.execute.tool.rye.agent.threads.thread_directive`| Execute thread_directive specifically     |
| `rye.search.directive.*`                             | Search all directives                     |
| `rye.load.knowledge.agency-kiwi.*`                   | Load any knowledge under `agency-kiwi/`   |
| `rye.sign.directive.*`                               | Sign any directive                        |

## Declaring Permissions in Directives

```xml
<permissions>
  <execute>
    <tool>rye.agent.threads.thread_directive</tool>
    <tool>rye.agent.threads.orchestrator</tool>
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
  ✓ thread_directive, orchestrator, search agency-kiwi.*, load agency-kiwi.*

  └── qualify_leads (declares own permissions):
      ✓ thread_directive, load agency-kiwi.*
      ✗ orchestrator (dropped), search (dropped)

      └── score_lead (declares own permissions):
          ✓ analysis.score_ghl_opportunity
          ✗ thread_directive (dropped), knowledge loading (dropped)

      └── leaf_without_permissions (no <permissions> block):
          ✓ Inherits qualify_leads: thread_directive, load agency-kiwi.*
```

## Design Principles

1. **Start with execution leaves** — each needs exactly the tools it calls
2. **Sub-orchestrators** need `thread_directive` + domain knowledge loading
3. **Root orchestrators** need `thread_directive`, `orchestrator` (wait/aggregate), search/load
4. **Never use `<permissions>*</permissions>` in production** — defeats the purpose
5. **LLM gets clear error** — denied permissions produce explicit messages for debugging
