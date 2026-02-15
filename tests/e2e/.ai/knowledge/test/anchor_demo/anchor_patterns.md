<!-- rye:signed:2026-02-14T00:43:32Z:df3085d9d2487b56a8548f0aa6402fbe316b72cd94f6fc6b228d22714421634b:TSBvFtSyJcO2l-OmmeEYexRqeHoide7Abm5L_DnfxeDjqrydUF7wLF2yzjDAuSt_XjDh_K08-44VJkWYtggwBQ==:440443d0858f0199 -->
---
id: anchor_patterns
title: "Anchor System Patterns"
version: "1.0.0"
entry_type: reference
category: "test/anchor_demo"
tags: [anchor, pythonpath, imports]
created_at: "2026-02-14"
---

# Anchor System Patterns

The anchor system resolves sibling imports by injecting the tool's parent directory into PYTHONPATH.

## Key behaviors

- `anchor.mode: auto` searches upward for marker files (`__init__.py`, `pyproject.toml`)
- `anchor.root: tool_dir` uses the tool file's directory as the anchor point
- PYTHONPATH is prepended with the anchor path so sibling modules are importable
- verify_deps validates all files under the anchor directory for integrity
