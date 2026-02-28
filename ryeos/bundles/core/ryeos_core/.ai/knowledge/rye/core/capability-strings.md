<!-- rye:signed:2026-02-28T00:32:39Z:7a0913ea901a85185616a04126281e1f77e9cbde5e0e3a682f3b0eb903c18b7c:mAO7-Vv3qn73VQSZL1iqRXenlI-Cw5U1JtVPeKGoTPz4QZq9Z_9CBMVpe1tdoUG10lgyzHXJXuQugqXnfti2Dg==:4b987fd4e40303ac -->
```yaml
name: capability-strings
title: Capability Strings & Permissions
entry_type: reference
category: rye/core
version: "1.1.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - capabilities
  - permissions
  - security
  - fnmatch
  - authorization
  - access-control
  - thread-permissions
  - risk-classification
  - acknowledge
references:
  - permissions-in-threads
  - "docs/orchestration/permissions-and-capabilities.md"
```

# Capability Strings & Permissions

How capability tokens control what threads can access.

## Capability String Format

```
rye.<primary>.<item_type>.<item_id_dotted>
```

| Segment           | Values                                         |
| ----------------- | ---------------------------------------------- |
| `rye`             | Fixed prefix                                   |
| `<primary>`       | `execute`, `search`, `load`, `sign`            |
| `<item_type>`     | `tool`, `directive`, `knowledge`               |
| `<item_id_dotted>`| Item ID with `/` replaced by `.`, supports fnmatch wildcards |

### Examples

| Capability String                                      | Allows                                    |
| ------------------------------------------------------ | ----------------------------------------- |
| `rye.execute.tool.rye.file-system.*`                   | Execute any tool under `rye/file-system/` |
| `rye.execute.tool.rye.agent.threads.thread_directive`  | Execute thread_directive (internal, used by `execute directive`) |
| `rye.execute.directive.domain.*`                       | Spawn threads for any directive under `domain/` |
| `rye.search.directive.*`                               | Search any directive                      |
| `rye.load.knowledge.agency-kiwi.*`                     | Load any knowledge under `agency-kiwi/`   |
| `rye.sign.directive.*`                                 | Sign any directive                        |

## Declaring Permissions in Directive XML

```xml
<permissions>
  <execute>
    <tool>rye.agent.threads.thread_directive</tool>
    <tool>rye.file-system.*</tool>
  </execute>
  <search>
    <directive>*</directive>
    <knowledge>agency-kiwi.*</knowledge>
  </search>
  <load>
    <knowledge>agency-kiwi.*</knowledge>
  </load>
  <sign>
    <directive>*</directive>
  </sign>
</permissions>
```

### XML → Capability String Conversion

| XML Declaration                                          | Capability String                                     |
| -------------------------------------------------------- | ----------------------------------------------------- |
| `<execute><tool>rye.file-system.*</tool></execute>`      | `rye.execute.tool.rye.file-system.*`                  |
| `<search><directive>*</directive></search>`              | `rye.search.directive.*`                              |
| `<load><knowledge>agency-kiwi.*</knowledge></load>`     | `rye.load.knowledge.agency-kiwi.*`                    |

Tag under the action (`<tool>`, `<directive>`, `<knowledge>`) specifies the item type. Text content is the item ID pattern.

## Wildcard Shortcuts

```xml
<!-- God mode — ALL permissions -->
<permissions>*</permissions>

<!-- All execute permissions -->
<execute>*</execute>

<!-- All search permissions -->
<search>*</search>
```

**Never use `<permissions>*</permissions>` in production.**

## The 4 Primary Actions

| Action    | What It Gates                                       |
| --------- | --------------------------------------------------- |
| `execute` | Running tools, spawning threads for directives, parsing knowledge via `rye_execute` |
| `search`  | Searching items via `rye_search`                    |
| `load`    | Loading/inspecting items via `rye_load`             |
| `sign`    | Signing items via `rye_sign`                        |

## Matching Algorithm

Uses Python `fnmatch` against the required capability string:

```python
required = f"rye.{primary}.{item_type}.{item_id.replace('/', '.')}"

for cap in self._capabilities:
    if fnmatch.fnmatch(required, cap):
        return None  # allowed

return {"error": f"Permission denied: '{required}' not covered..."}
```

- `*` matches anything within a segment
- `?` matches a single character
- Item IDs use `/` separators → capabilities use `.` separators

## Fail-Closed Default

**If no capabilities are declared, ALL actions are denied.**

A directive with an empty or missing `<permissions>` block cannot execute any tools, load any knowledge, or search for anything.

## Internal Tool Bypass

Tools under `rye/agent/threads/internal/*` are **always allowed** — no permission check needed. These include limit_checker, cost_tracker, cancel_checker, etc.

## Capability Attenuation (Thread Hierarchy)

Capabilities flow down the thread hierarchy. Children can have the same or fewer capabilities — never more.

| Directive Permissions | Parent Capabilities | Result                         |
| --------------------- | ------------------- | ------------------------------ |
| Declared              | Any                 | Uses directive's permissions   |
| Not declared          | Inherited from parent | Uses parent's capabilities   |
| Not declared          | None (root thread)  | Empty → all actions denied     |

## Principle of Least Privilege

Design permissions bottom-up:

1. **Execution leaves** — exactly the tools they call
2. **Sub-orchestrators** — `thread_directive` (internal) + `directive` patterns for children + knowledge they load
3. **Root orchestrators** — `thread_directive` (internal) + `orchestrator` + `directive` patterns + domain search/load
4. **Never `*` in production** — defeats the purpose

> **Note:** The primary way to spawn threads is `execute directive`. This internally requires the `rye.execute.tool.rye.agent.threads.thread_directive` capability, so orchestrators still need that tool permission declared.

## Permission Check Flow

The runner checks permissions before every tool call dispatch:

```python
inner_primary = tc_name.replace("rye_", "", 1)   # "rye_execute" → "execute"
inner_item_type = tc_input.get("item_type", "tool")
inner_item_id = tc_input.get("item_id", "")

denied = harness.check_permission(inner_primary, inner_item_type, inner_item_id)
if denied:
    # Error returned to the LLM as tool result
    messages.append({"role": "tool", "content": str(denied)})
    continue  # skip execution
```

The LLM receives a clear error message explaining exactly which capability is missing.

## Risk Classification

Capability strings are classified by risk level to enforce graduated access control. Classifications are defined in `capability_risk.yaml`.

### Risk Tiers

| Tier           | Policy                 | Description                                         |
|----------------|------------------------|-----------------------------------------------------|
| `safe`         | `allow`                | Read-only operations (search, load)                 |
| `write`        | `allow`                | State-modifying but routine (file-system tools)     |
| `elevated`     | `acknowledge_required` | High-impact — requires `<acknowledge>` opt-in       |
| `unrestricted` | `block`                | Full access — blocked by default                    |

Matching uses most-specific-first: `rye.search.*` (safe, 3 segments) wins over `rye.*` (unrestricted, 2 segments) for a search capability.

See `rye/agent/threads/permissions-in-threads` for the full risk classification model, policies, and matching algorithm.

## The `<acknowledge>` Tag

Directives requesting capabilities classified as `elevated` must include an `<acknowledge>` tag in their `<permissions>` block:

```xml
<permissions>
  <acknowledge>elevated</acknowledge>
  <execute>
    <directive>*</directive>
  </execute>
</permissions>
```

Without `<acknowledge>`, elevated capabilities are denied even if the capability string pattern matches. This prevents accidental use of high-impact operations.

See `rye/agent/threads/permissions-in-threads` for details on risk levels, policies, and broad capability warnings.
