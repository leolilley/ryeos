```yaml
id: load
title: "rye_load"
description: Load item content for inspection or copy between locations
category: tools-reference
tags: [load, mcp-tool, api]
version: "1.0.0"
```

# rye_load

Load the raw content and metadata of a directive, tool, or knowledge item. Optionally copy items between spaces (project, user, system) for customization.

## Parameters

| Parameter      | Type   | Required | Default     | Description                                                                         |
| -------------- | ------ | -------- | ----------- | ----------------------------------------------------------------------------------- |
| `item_type`    | string | yes      | —           | `"directive"`, `"tool"`, or `"knowledge"`                                           |
| `item_id`      | string | yes      | —           | Relative path from `.ai/<type>/` without extension (e.g., `"rye/core/create_tool"`) |
| `project_path` | string | yes      | —           | Absolute path to the project root                                                   |
| `source`       | string | no       | —           | Where to find the item: `"project"`, `"user"`, or `"system"`. When omitted, cascades **project → user → system** (first match wins). |
| `destination`  | string | no       | —           | If set and different from `source`, copies the item to this space                   |

## Response

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

## Integrity Verification

Items are verified against their signature when loaded. If the content has been modified since signing, the load fails with an integrity error.

## Copying Between Spaces

Set `destination` to copy an item from one space to another. This is useful for pulling system-provided items into your project for customization.

When copying, the file is placed in the destination's type directory, preserving the filename. The response includes both the original path and the copy destination:

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

## Error Responses

```json
{
  "status": "error",
  "error": "Item not found: my-project/missing-tool",
  "item_type": "tool",
  "item_id": "my-project/missing-tool"
}
```

## Examples

### Load a directive from the project space

```python
rye_load(
    item_type="directive",
    item_id="rye/core/create_directive",
    project_path="/home/user/my-project",
    source="project"
)
```

### Load a system tool for inspection

```python
rye_load(
    item_type="tool",
    item_id="rye/file-system/read",
    project_path="/home/user/my-project",
    source="system"
)
```

### Copy a system directive into the project for customization

```python
rye_load(
    item_type="directive",
    item_id="rye/core/create_tool",
    project_path="/home/user/my-project",
    source="system",
    destination="project"
)
```

### Load a knowledge entry

```python
rye_load(
    item_type="knowledge",
    item_id="rye/core/tool-metadata-reference",
    project_path="/home/user/my-project",
    source="system"
)
```
