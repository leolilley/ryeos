---
category: ryeos/core/handlers
tags: [handler, parser, markdown, frontmatter]
version: "1.0.0"
description: YAML header plus body parser handler reference.
---

# Handler: yaml-header-document

Invariant: `yaml-header-document` extracts structured YAML metadata plus remaining body text from markdown-like files.

It supports frontmatter and fenced YAML forms. The markdown knowledge and directive parsers use it to preserve prompt/body text while exposing typed metadata to the kind composer.
