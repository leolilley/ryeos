---
category: ryeos/core/parsers
tags: [parser, markdown, frontmatter, knowledge]
version: "1.0.0"
description: Markdown frontmatter parser reference.
---

# Parser: markdown/frontmatter

Invariant: `parser:ryeos/core/markdown/frontmatter` extracts YAML metadata and markdown body from knowledge files.

It uses `handler:ryeos/core/yaml-header-document`, accepts markdown frontmatter/fenced YAML forms, and preserves the body for runtime context composition.
