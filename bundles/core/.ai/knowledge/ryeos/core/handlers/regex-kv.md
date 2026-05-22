<!-- ryeos:signed:2026-05-22T03:35:36Z:bc30fa4564db44a7410fae141eeefc90e36eb749448b712cf8d0732f1f4a4682:uurWsqbtBbaeH2tGlccX3TQpDnkT9sUdt1ZadvavMH7TBSL8GZDbQehxwZ86DJF7+bayKt/fTn68EpZ1VXfhCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/handlers
tags: [handler, parser, regex]
version: "1.0.0"
description: Regex key-value parser handler reference.
---

# Handler: regex-kv

Invariant: `regex-kv` extracts named metadata fields from source text using configured regular expressions.

It backs lightweight source parsers such as JavaScript constants and Python dunder metadata. Parser descriptors provide the patterns, key normalization, and output schema; the handler only performs extraction and returns a mapping.
