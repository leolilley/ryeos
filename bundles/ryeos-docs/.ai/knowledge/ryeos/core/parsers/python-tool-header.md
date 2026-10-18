<!-- ryeos:signed:2026-09-06T01:55:11Z:e3d7b69fe40a78a6ada87e7f99cb13c32fa047d9d73401b35bbdfcabce4561b6:ooOIFMs4QUZXo4G7IuLDcIi5pCjan+Y5Eq5QvBXm9Yua++xU9LT0fDfroqa73i1yiygng7ig3Z3mCoMEM5MRCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
# ryeos:signed:2026-06-07T05:37:38Z:77c236456c3551d0bcd7294db09fa5ee4022863137ccd3ebdce10ede34440704:hfxQqX7PIeIZ9TTOo4eCnjvThQ8g8qGiAx8gJU6j2KmFXliyuYz4IcxthH5CS0ZL0Nmx7q6M6INOx78KM6wGDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
---
category: ryeos/core/parsers
tags: [parser, python, metadata, tools]
version: "1.1.0"
description: Python tool-header parser reference.
---

# Parser: python/tool-header

Invariant: `parser:ryeos/core/python/tool-header` extracts Python tool metadata from a `# ryeos-tool:` comment-YAML header without executing the file.

It is bound through the parser registry and feeds the `tool` kind for `.py` files.

The header must appear in the file prologue, after an optional shebang
and after any Rye OS signature line has been stripped by the parser
dispatcher:

```python
#!/usr/bin/env python3
# ryeos-tool:
#   category: my/project
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/function"
#   description: "Run a Python function tool"

def execute(params, project_path):
    return {"ok": True}
```

The parser uses `handler:ryeos/core/yaml-header-document` with its
`comment_marker` form. It returns the inner mapping under `ryeos-tool`,
so downstream `metadata.rules` see the same plain-key shape as YAML tool
descriptors.
