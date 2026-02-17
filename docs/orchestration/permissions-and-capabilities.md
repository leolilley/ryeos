---
id: permissions-and-capabilities
title: "Permissions and Capabilities"
description: How capability tokens control what threads can do
category: orchestration
tags: [permissions, capabilities, security, fail-closed]
version: "1.0.0"
---

# Permissions and Capabilities

Every thread is constrained by a set of capability tokens that determine which tools, directives, and knowledge items it can access. Capabilities are declared in the directive XML, enforced by the `SafetyHarness`, and attenuated as they flow down the thread hierarchy.

## Capability String Format

Capabilities follow this structure:

```
rye.<primary>.<item_type>.<item_id_dotted>
```

Where:
- **primary** — the action: `execute`, `search`, `load`, `sign`
- **item_type** — what you're acting on: `tool`, `directive`, `knowledge`
- **item_id_dotted** — the item ID with `/` separators replaced by `.`, supporting fnmatch wildcards

Examples:

| Capability | Allows |
|-----------|--------|
| `rye.execute.tool.rye.file-system.*` | Execute any tool under `rye/file-system/` |
| `rye.execute.tool.rye.agent.threads.thread_directive` | Execute the thread_directive tool specifically |
| `rye.search.directive` | Search directives (search has no item_id) |
| `rye.load.knowledge.agency-kiwi.*` | Load any knowledge under `agency-kiwi/` |
| `rye.sign.directive.*` | Sign any directive |

## Declaring Permissions in Directives

Permissions are declared in the directive's XML `<permissions>` block using a hierarchical structure:

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

Each inner element declares a capability. The tag under the action (`<tool>`, `<directive>`, `<knowledge>`) specifies the item type, and the text content is the item ID pattern.

### How XML Becomes Capability Strings

The parser extracts permissions as `{tag: "cap", content: "rye.<primary>.<item_type>.<pattern>"}`. The `SafetyHarness` converts these to capability strings by replacing `/` with `.`:

| XML Declaration | Capability String |
|----------------|-------------------|
| `<execute><tool>rye.file-system.*</tool></execute>` | `rye.execute.tool.rye.file-system.*` |
| `<execute><tool>rye.agent.threads.thread_directive</tool></execute>` | `rye.execute.tool.rye.agent.threads.thread_directive` |
| `<search><directive>*</directive></search>` | `rye.search.directive.*` |
| `<load><knowledge>agency-kiwi.*</knowledge></load>` | `rye.load.knowledge.agency-kiwi.*` |

### Wildcard Shortcuts

For directives that need broad access:

```xml
<!-- All permissions (god mode) -->
<permissions>*</permissions>

<!-- All execute permissions -->
<execute>*</execute>

<!-- All search permissions -->
<search>*</search>
```

## Fail-Closed Default

**If no capabilities are declared, ALL actions are denied.** This is a security-critical design choice.

```python
if not self._capabilities:
    return {
        "error": f"Permission denied: no capabilities declared. "
                 f"Cannot {primary} {item_type} '{target}'",
        ...
    }
```

A directive with an empty or missing `<permissions>` block can't execute any tools, load any knowledge, or search for anything. This prevents accidental privilege escalation — you must explicitly declare what the directive needs.

## Permission Checking Flow

The runner checks permissions before every tool call dispatch:

```python
# runner.py — inside the tool call loop
inner_primary = tc_name.replace("rye_", "", 1)   # "rye_execute" → "execute"
inner_item_type = tc_input.get("item_type", "tool")
inner_item_id = tc_input.get("item_id", "")

denied = harness.check_permission(inner_primary, inner_item_type, inner_item_id)
if denied:
    # Return error to the LLM as the tool result
    messages.append({
        "role": "tool",
        "tool_call_id": tool_call["id"],
        "content": str(denied),
    })
    continue  # skip execution, LLM sees the denial
```

The LLM receives the permission denial as a tool result and can react accordingly (e.g., report the error, try a different approach).

### Check Algorithm

```python
def check_permission(self, primary, item_type, item_id=""):
    # Internal thread tools are always allowed
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
        # search has no item_id
        required = f"rye.{primary}.{item_type}"

    # Check against all capabilities using fnmatch
    for cap in self._capabilities:
        if fnmatch.fnmatch(required, cap):
            return None  # allowed

    return {"error": f"Permission denied: '{required}' not covered..."}
```

Key details:
- **fnmatch wildcards** — `*` matches anything within a single segment, `?` matches a single character
- **Item ID conversion** — `/` in item IDs becomes `.` in capability strings for matching
- **Internal tools always allowed** — `rye/agent/threads/internal/*` tools (limit_checker, cost_tracker, etc.) bypass permission checks

## Capability Attenuation

Capabilities flow down the thread hierarchy with attenuation — children can have the same or fewer capabilities than their parent, never more.

### How Child Capabilities Are Derived

```python
# SafetyHarness.__init__
child_caps = []
if permissions:
    # Directive declares its own permissions → use those
    child_caps = [p["content"].replace("/", ".") for p in permissions if p.get("tag") == "cap"]

if child_caps:
    self._capabilities = child_caps
elif parent_capabilities:
    # No permissions declared → inherit parent's capabilities
    self._capabilities = [c.replace("/", ".") for c in parent_capabilities]
else:
    # No permissions, no parent → empty (fail-closed)
    self._capabilities = []
```

Three scenarios:

| Directive Permissions | Parent Capabilities | Result |
|----------------------|--------------------|---------| 
| Declared | Any | Uses directive's permissions |
| Not declared | Inherited from parent | Uses parent's capabilities |
| Not declared | None (root thread) | Empty → all actions denied |

### Attenuation in Practice

Consider this hierarchy:

**Root orchestrator** declares:
```xml
<permissions>
  <execute>
    <tool>rye.agent.threads.thread_directive</tool>
    <tool>rye.agent.threads.orchestrator</tool>
  </execute>
  <search><directive>agency-kiwi.*</directive></search>
  <load><knowledge>agency-kiwi.*</knowledge></load>
</permissions>
```

Capabilities: can spawn threads, search agency-kiwi directives, load agency-kiwi knowledge.

**Sub-orchestrator `qualify_leads`** declares:
```xml
<permissions>
  <execute>
    <tool>rye.agent.threads.thread_directive</tool>
  </execute>
  <load><knowledge>agency-kiwi.*</knowledge></load>
</permissions>
```

Capabilities: can spawn threads and load knowledge. **Cannot** use `orchestrator` operations or search directives — those capabilities were dropped.

**Execution leaf `score_lead`** declares:
```xml
<permissions>
  <execute>
    <tool>analysis.score_ghl_opportunity</tool>
  </execute>
</permissions>
```

Capabilities: can execute exactly one tool. **Cannot** spawn threads, load knowledge, or search anything. Minimal privilege for a leaf that does one thing.

**Execution leaf without permissions:**
```xml
<!-- No <permissions> block -->
```

Inherits parent's capabilities. If spawned by `qualify_leads`, it can spawn threads and load knowledge. This is the inheritance fallback — useful when you want children to have the same access as their parent.

## Real Permission Declarations

### Root Orchestrator

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

Needs `thread_directive` to spawn children, `orchestrator` to wait/aggregate, and search/load for its domain knowledge.

### Discovery Leaf

```xml
<permissions>
  <execute>
    <tool>scraping.gmaps.scrape_gmaps</tool>
  </execute>
  <load>
    <knowledge>agency-kiwi.*</knowledge>
  </load>
</permissions>
```

Can execute exactly one scraping tool and load knowledge for context. Cannot spawn threads or search.

### Scoring Leaf

```xml
<permissions>
  <execute>
    <tool>analysis.score_ghl_opportunity</tool>
  </execute>
</permissions>
```

The tightest possible scope: one tool, no knowledge loading, no searching. The LLM calls the scoring tool and returns — nothing else is permitted.

## Principle of Least Privilege

Design permissions from the bottom up:

1. **Start with execution leaves** — each needs exactly the tools it calls
2. **Sub-orchestrators** need `thread_directive` plus whatever knowledge they load
3. **Root orchestrators** need `thread_directive`, `orchestrator` (for wait/aggregate), and their domain's search/load
4. **Never use `<permissions>*</permissions>`** in production — it defeats the purpose

If a thread tries something it shouldn't, the LLM gets a clear error message explaining exactly which capability is missing. This makes debugging permission issues straightforward.

## What's Next

- [Continuation and Resumption](./continuation-and-resumption.md) — How threads handle context limits
- [Building a Pipeline](./building-a-pipeline.md) — Putting it all together
