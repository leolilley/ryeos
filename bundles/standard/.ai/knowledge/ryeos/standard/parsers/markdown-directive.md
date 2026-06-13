<!-- ryeos:signed:2026-06-11T21:03:05Z:eaa55f1d9252e1c5c03a4ef6b33268f0853c8e90467d25932e2f43b805b8fc5f:Jk8fAWE2IbOMkzRhUZQB6XUAPLx67hG+/cYw9i2pkNYioc/UlziTGiqbJfjyteSyFEfbNj8SvJvA1vFKziZlDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/parsers
tags: [parser, markdown, directive]
version: "1.0.0"
description: Directive markdown parser reference.
---

# Parser: markdown/directive

Invariant: the directive markdown parser extracts YAML metadata plus the prompt body from signed directive `.md` files.

It supports the directive kind's HTML-comment signature envelope, preserves body text for `root_verbatim` composition, and exposes frontmatter fields such as `extends`, `permissions`, `context`, `model`, `limits`, inputs, outputs, and actions.
