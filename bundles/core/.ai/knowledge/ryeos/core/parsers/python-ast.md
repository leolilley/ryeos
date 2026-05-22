<!-- ryeos:signed:2026-05-22T04:30:07Z:0a9634e769ae790e26714eaec6582d3603fe736544ac363403c1554fc1805641:D5YKRvR2yYLUD2+tPj9pEVV/dN2ZzaeAvIKKwxxLQ6zr4v6iySYtmOtt68Lo+1JaG/hyO6s9mGV00CmO7VjkBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/parsers
tags: [parser, python, metadata]
version: "1.0.0"
description: Python AST parser reference.
---

# Parser: python/ast

Invariant: `parser:ryeos/core/python/ast` extracts Python tool metadata from dunder constants and docstrings without executing the file.

It is bound through the parser registry and feeds the `tool` and `streaming_tool` kinds for `.py` files.
