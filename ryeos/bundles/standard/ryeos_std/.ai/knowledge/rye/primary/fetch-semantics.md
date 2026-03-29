<!-- rye:signed:2026-03-29T06:39:14Z:470b40123d1214bb41a9d3ea5bb490fdb1f56ea80aa06ca9fc735a7af992e852:jubpyc8in5xg1D0A3spOxnwrVaZHGfHf6xf7Mb74oVKLmM3GlpRQ0dOs8Zxr-C_NNW-iZca_B3VKrD98OMNDAQ==:4b987fd4e40303ac -->
```yaml
name: fetch-semantics
title: "rye_fetch — MCP Tool Semantics"
entry_type: reference
category: rye/primary
version: "1.0.0"
author: rye-os
created_at: 2026-03-29T00:00:00Z
tags:
  - fetch
  - mcp-tool
  - api
references:
  - execute-semantics
  - sign-semantics
  - "docs/tools-reference/fetch.md"
```

# rye_fetch — MCP Tool Semantics

Resolve items by ID or discover items by query. Two modes of one operation.

## Parameters

| Parameter      | Type    | Required | Default | Description                                                |
| -------------- | ------- | -------- | ------- | ---------------------------------------------------------- |
| `item_id`      | string  | no*      | —       | Slash-separated path without extension. Triggers ID mode.  |
| `item_type`    | string  | no       | —       | Restrict to type (ID mode only). Auto-detects if omitted.  |
| `query`        | string  | no*      | —       | Keyword search. Triggers query mode.                       |
| `scope`        | string  | no       | —       | Item type + namespace filter (query mode).                 |
| `project_path` | string  | yes      | —       | Absolute path to project root containing `.ai/`.           |
| `source`       | string  | no       | varies  | Restrict resolution source.                                |
| `destination`  | string  | no       | —       | Copy item after resolving (ID mode only).                  |
| `version`      | string  | no       | —       | Version to pull (registry source only).                    |
| `limit`        | integer | no       | 10      | Max results (query mode only).                             |

*Either `item_id` or `query` must be provided. Not both.

## Mode Detection

| Parameters provided | Mode       | Behavior                  |
| ------------------- | ---------- | ------------------------- |
| `item_id`           | ID mode    | Return content + metadata |
| `query` + `scope`   | Query mode | Return matching items     |
| Both                | Error      | Ambiguous                 |
| Neither             | Error      | Nothing to resolve        |

## ID Mode

Resolves an item by exact path. Cascades project → user → system.

- If `item_type` omitted: tries directive → tool → knowledge
  - Exactly one match → returns it
  - Multiple matches → ambiguity error listing types
  - No matches → not found error
- If `source="registry"`: `item_type` is required
- Hard errors (integrity, auth) bubble up immediately during auto-detect
- Response always includes `item_type`, `source`, `mode: "id"`

## Query Mode

Discovers items matching keyword search.

- `item_type` is rejected in query mode — use `scope` instead
- Supports: AND, OR, NOT, wildcards (*), quoted phrases, fuzzy matching
- BM25-inspired field-weighted scoring
- Shadow detection across spaces
- Response includes `mode: "query"`

## Source Values

| Value      | ID Mode | Query Mode | Meaning                            |
| ---------- | ------- | ---------- | ---------------------------------- |
| `project`  | ✓       | ✓          | Project space only                 |
| `user`     | ✓       | ✓          | User space only                    |
| `system`   | ✓       | ✓          | System bundles only                |
| `local`    | —       | ✓          | All local spaces                   |
| `registry` | ✓       | ✓          | Remote registry only               |
| `all`      | —       | ✓          | Local + registry (query default)   |
| (omitted)  | cascade | `all`      | ID: project→user→system. Query: all|

## Copying Items (ID Mode Only)

Set `destination` to `"project"` or `"user"` to copy after resolving.
Always re-sign after copying.
