<!-- ryeos:signed:2026-05-23T09:45:40Z:eaa55f1d9252e1c5c03a4ef6b33268f0853c8e90467d25932e2f43b805b8fc5f:zEmZeZpadkO48r+y32d1wltslWjAkfHa/KuSDuIh2D+VXWPkRQF/QC2uwjc0ax9yKFVgvj7WjvgGXl4r2Ss6DA==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
category: ryeos/standard/parsers
tags: [parser, markdown, directive]
version: "1.0.0"
description: Directive markdown parser reference.
---

# Parser: markdown/directive

Invariant: the directive markdown parser extracts YAML metadata plus the prompt body from signed directive `.md` files.

It supports the directive kind's HTML-comment signature envelope, preserves body text for `root_verbatim` composition, and exposes frontmatter fields such as `extends`, `permissions`, `context`, `model`, `limits`, inputs, outputs, and actions.
