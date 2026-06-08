<!-- ryeos:signed:2026-06-08T00:42:19Z:fc8dd3b582ee25bd9d348bad0cbd787f380ad077e63ee5c36dc2729e34cd0810:rHNVl5AyZhWAFYGs/hL8LIGolvdFYXoM8BOy0pXJDK6/7S/fxYrvseSItiFtoA1ZjikHJTZvMfcxrODB5FmGBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
# ryeos:signed:2026-06-07T05:37:38Z:77c236456c3551d0bcd7294db09fa5ee4022863137ccd3ebdce10ede34440704:hfxQqX7PIeIZ9TTOo4eCnjvThQ8g8qGiAx8gJU6j2KmFXliyuYz4IcxthH5CS0ZL0Nmx7q6M6INOx78KM6wGDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
---
category: ryeos/core/parsers
tags: [parser, python, metadata, tools]
version: "1.0.0"
description: Python tool-header parser reference.
---

# Parser: python/tool-header

Invariant: `parser:ryeos/core/python/tool-header` extracts Python tool metadata from a `# ryeos-tool:` comment-YAML header without executing the file.

It is bound through the parser registry and feeds the `tool` and `streaming_tool` kinds for `.py` files.

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
