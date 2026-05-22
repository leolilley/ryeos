<!-- ryeos:signed:2026-05-22T07:21:24Z:ba3cfd79784fa503b6b4a7be49f31d0099c31975397f9ad30eb64efa1ef473df:/ZuYre9xKaNFLVEHqXZyzB6+UpE0WU3Zwg8TS+1VeyNC7wivBrAQKkQqFmrbvCuhTMnKAbl6JyIvb9YTTqUJAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/parsers
tags: [parser, markdown, frontmatter, knowledge]
version: "1.0.0"
description: Markdown frontmatter parser reference.
---

# Parser: markdown/frontmatter

Invariant: `parser:ryeos/core/markdown/frontmatter` extracts YAML metadata and markdown body from knowledge files.

It uses `handler:ryeos/core/yaml-header-document`, accepts markdown frontmatter/fenced YAML forms, and preserves the body for runtime context composition.
