---
category: ryeos/core/kinds
tags: [kind, parser, formats]
version: "1.0.0"
description: Parser kind reference.
---

# Kind: parser

Invariant: `parser` items bind source formats to handler binaries and parser configuration.

- Directory: `parsers/`
- Formats: `.yaml` via YAML parser
- Composer: identity
- Execution: none
- Descriptor validation: strongly typed by the parser registry after YAML parsing

Parser descriptors name a handler, parser API version, parser config, and output schema. Kind schemas refer to parsers from their format table.
