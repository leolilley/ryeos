<!-- ryeos:signed:2026-05-22T04:30:07Z:932f0ecbfa9a4d8c08e63c8d81a74eddaa7266878fd946dac9f8c0f72b5a2591:wTpl8eJkKUfRKfr2fEaX7t4aQwqRJGvSwy9dzZ5oTpz4GECmKxHHVJlcBKzujD440a81Jno+GlTe/jrkBWvXDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
