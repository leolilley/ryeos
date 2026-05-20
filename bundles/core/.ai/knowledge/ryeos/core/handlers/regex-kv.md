---
category: ryeos/core/handlers
tags: [handler, parser, regex]
version: "1.0.0"
description: Regex key-value parser handler reference.
---

# Handler: regex-kv

Invariant: `regex-kv` extracts named metadata fields from source text using configured regular expressions.

It backs lightweight source parsers such as JavaScript constants and Python dunder metadata. Parser descriptors provide the patterns, key normalization, and output schema; the handler only performs extraction and returns a mapping.
