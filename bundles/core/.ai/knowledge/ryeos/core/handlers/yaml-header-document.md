<!-- ryeos:signed:2026-05-22T04:30:07Z:5e73153bfbd8e6e16d22e543f86fdf54367ce9ab88c5f28fcc29380e2e20b330:5ED0KXNA4MsTLIwUbV+8lNGc3AcUsHwzwgUS487/8fRsrsPpt4lztpyGyRug/82+qkR/xt4V0hhEDXKIi8Z8Bw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/handlers
tags: [handler, parser, markdown, frontmatter]
version: "1.0.0"
description: YAML header plus body parser handler reference.
---

# Handler: yaml-header-document

Invariant: `yaml-header-document` extracts structured YAML metadata plus remaining body text from markdown-like files.

It supports frontmatter and fenced YAML forms. The markdown knowledge and directive parsers use it to preserve prompt/body text while exposing typed metadata to the kind composer.
