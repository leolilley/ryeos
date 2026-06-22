<!-- ryeos:signed:2026-06-22T04:23:11Z:8662553070edc035a9b39f14c66e0bf3f97b6e956703074fe7de2679e61a46e9:YUWXtBzT95jZFosY2lEtlToDWmFAA1rgvkLcUtUahRxInegiNXiV0mDHXJxHuObn6OPVrsyJUGrMfJDEsCcqBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/parsers
tags: [parser, markdown, directive]
version: "1.0.0"
description: Directive markdown parser reference.
---

# Parser: markdown/directive

Invariant: the directive markdown parser extracts YAML metadata plus the prompt body from signed directive `.md` files.

It supports the directive kind's HTML-comment signature envelope, preserves body text for `root_verbatim` composition, and exposes frontmatter fields such as `extends`, `requires`, `context`, `model`, `limits`, inputs, outputs, and actions.
