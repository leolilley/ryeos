<!-- rye:signed:2026-02-23T00:43:10Z:081921dabd977de02cca04a8e58070f9803c15cc9876420e94bbd6e01d7f9c17:LC1dgm3ZzKcULSR_4Xa2VR9PtcMkHe-_uY8Bpk2XduQ1sN5GdGCAKhw9SaYHfcaxUPWH_JNbjVbXa50y9dPBBg==:9fbfabe975fa5a7f -->

```yaml
id: load-semantics
title: "rye_load — MCP Tool Semantics"
entry_type: reference
category: rye/primary
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - load
  - mcp-tool
  - api
references:
  - execute-semantics
  - search-semantics
  - sign-semantics
  - "docs/tools-reference/load.md"
```

# rye_load — MCP Tool Semantics

Load the raw content and metadata of a directive, tool, or knowledge item. Optionally copy items between spaces for customization.

## Parameters

| Parameter      | Type   | Required | Default | Description                                                                                                                  |
| -------------- | ------ | -------- | ------- | ---------------------------------------------------------------------------------------------------------------------------- |
| `item_type`    | string | yes      | —       | `"directive"`, `"tool"`, or `"knowledge"`                                                                                    |
| `item_id`      | string | yes      | —       | Relative path from `.ai/<type>/` without extension                                                                           |
| `project_path` | string | yes      | —       | Absolute path to the project root                                                                                            |
| `source`       | string | no       | —       | Where to find the item: `"project"`, `"user"`, or `"system"`. When omitted, cascades project→user→system (first match wins). |
| `destination`  | string | no       | —       | If set and different from `source`, copies the item to this space                                                            |

## Load vs Execute

| Aspect       | `rye_load`                               | `rye_execute`                                  |
| ------------ | ---------------------------------------- | ---------------------------------------------- |
| **Returns**  | Raw file content + metadata              | Parsed/interpolated data + execution results   |
| **Purpose**  | Inspect, read, or copy items             | Run directives, execute tools, parse knowledge |
| **Parsing**  | None — returns content as-is             | Full parsing, validation, and interpolation    |
| **Use case** | "Show me the source" / "Copy to project" | "Run this directive" / "Execute this tool"     |

## Integrity Verification

Items are verified against their signature when loaded. Modified content since signing → integrity error.

## Response Format

```json
{
  "status": "success",
  "content": "<!-- rye:signed:... -->\n# Create Tool\n...",
  "metadata": {
    "name": "create_tool",
    "path": "/home/user/.ai/directives/rye/core/create_tool.md",
    "extension": ".md",
    "version": "1.0.0"
  },
  "path": "/home/user/.ai/directives/rye/core/create_tool.md",
  "source": "user"
}
```

### Metadata fields

| Field       | Description                                                |
| ----------- | ---------------------------------------------------------- |
| `name`      | Filename stem (without extension)                          |
| `path`      | Absolute path to the source file                           |
| `extension` | File extension (`.md`, `.py`, `.yaml`, etc.)               |
| `version`   | Extracted from `__version__` or `version="..."` if present |

## Copying Between Spaces

Set `destination` to copy an item from one space to another. The file is placed in the destination's type directory, preserving the filename.

**Copy response includes extra fields:**

```json
{
  "status": "success",
  "content": "...",
  "metadata": { "..." },
  "path": "/path/to/rye/package/.ai/tools/rye/bash/bash.py",
  "source": "system",
  "copied_to": "project",
  "destination_path": "/home/user/my-project/.ai/tools/bash.py"
}
```

### Valid copy directions

| Source    | Destination | Use case                                    |
| --------- | ----------- | ------------------------------------------- |
| `system`  | `project`   | Customize a built-in item for your project  |
| `system`  | `user`      | Customize a built-in item globally          |
| `user`    | `project`   | Override a user-level item for this project |
| `project` | `user`      | Promote a project item to your global space |

## Error Response

```json
{
  "status": "error",
  "error": "Item not found: my-project/missing-tool",
  "item_type": "tool",
  "item_id": "my-project/missing-tool"
}
```

## Usage Examples

```python
# Load a directive from project space
rye_load(
    item_type="directive",
    item_id="rye/core/create_directive",
    project_path="/home/user/my-project",
    source="project"
)

# Load a system tool for inspection
rye_load(
    item_type="tool",
    item_id="rye/file-system/read",
    project_path="/home/user/my-project",
    source="system"
)

# Copy system directive into project for customization
rye_load(
    item_type="directive",
    item_id="rye/core/create_tool",
    project_path="/home/user/my-project",
    source="system",
    destination="project"
)

# Load a knowledge entry
rye_load(
    item_type="knowledge",
    item_id="rye/core/tool-metadata-reference",
    project_path="/home/user/my-project",
    source="system"
)
```
