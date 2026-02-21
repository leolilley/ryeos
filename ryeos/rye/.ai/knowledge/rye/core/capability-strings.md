<!-- rye:signed:2026-02-21T05:56:40Z:375eb6d1df8bc42f861a4fa88d0d108d903cc03512f38f7956495e249397f10e:wph7MKyUzD_5-K5mfJjL0N3X4c4ffY6-R6UuPDuK1g1mrOx4gWpA4bxcEuWCfmbEtkKAMXWu1PB1OkavyMrXBg==:9fbfabe975fa5a7f -->

```yaml
id: capability-strings
title: Capability Strings & Permissions
entry_type: reference
category: rye/core
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - capabilities
  - permissions
  - security
references:
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
| `rye.execute.tool.rye.agent.threads.thread_directive`  | Execute thread_directive specifically     |
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
| `execute` | Running tools, directives, knowledge via `rye_execute` |
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
2. **Sub-orchestrators** — `thread_directive` + knowledge they load
3. **Root orchestrators** — `thread_directive` + `orchestrator` + domain search/load
4. **Never `*` in production** — defeats the purpose

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
