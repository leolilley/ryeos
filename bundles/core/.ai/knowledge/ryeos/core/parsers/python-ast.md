---
category: ryeos/core/parsers
tags: [parser, python, metadata]
version: "1.0.0"
description: Python AST parser reference.
---

# Parser: python/ast

Invariant: `parser:ryeos/core/python/ast` extracts Python tool metadata from dunder constants and docstrings without executing the file.

It is bound through the parser registry and feeds the `tool` and `streaming_tool` kinds for `.py` files.
