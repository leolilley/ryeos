---
category: ryeos/standard/parsers
tags: [parser, markdown, directive]
version: "1.0.0"
description: Directive markdown parser reference.
---

# Parser: markdown/directive

Invariant: the directive markdown parser extracts YAML metadata plus the prompt body from signed directive `.md` files.

It supports the directive kind's HTML-comment signature envelope, preserves body text for `root_verbatim` composition, and exposes frontmatter fields such as `extends`, `permissions`, `context`, `model`, `limits`, inputs, outputs, and actions.
