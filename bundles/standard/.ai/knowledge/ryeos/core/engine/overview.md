<!-- ryeos:signed:2026-07-16T03:44:58Z:dc04c6f6e139d322077100a898902acbf078b761dfc477a15c5f0799d65673b4:geO0JMUHjT2TNNhFsnlX3gVhsuCaRgjSG2zBJFSH2Y+3VSbFi+HQvFk6MTfa6EhfpZToFB1zJKRXaz5udXY0Cg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/engine
tags: [engine, architecture, parse, compose, execute]
version: "1.2.0"
description: >
  Engine architecture and the parse → compose → plan → execute pipeline.
---

# Engine Overview

Invariant: the engine turns a canonical ref into a verified, composed item and an execution plan without hard-coding item-specific behavior in the dispatch loop.

## Pipeline

1. Parse the canonical ref (`kind:path/to/item`) and load the kind schema.
2. Resolve the item across project, user, then installed bundle spaces.
3. Select the parser declared by the kind's `formats` entry for the file extension.
4. Parse metadata/body into the kind's composed-value contract.
5. Compose with the kind's handler (`identity`, `extends-chain`, or a domain composer).
6. Verify signature and trust at the boundary that requires trust.
7. Build a plan from the kind execution block: in-process service, subprocess tool, operation dispatch, or runtime-registry delegation.
8. For executable tool/runtime plans, apply the immutable node isolation snapshot
   before the Lillux spawn boundary.

Core owns the generic machine: config, handler, parser, protocol, runtime, service, node, tool, and streaming_tool kinds. Standard contributes workflow kinds and runtimes such as directive, graph, and knowledge.

## Extension points

- **Kind schemas** define directory mapping, accepted formats, composer, metadata extraction, and execution model.
- **Parsers** bind a format to a handler binary and parser config.
- **Handlers** are parser/composer binaries loaded during bootstrap.
- **Protocols** define subprocess wire contracts.
- **Runtimes** are runtime binaries serving delegated workflow kinds.
- **Services** are in-process daemon endpoints.

The engine stays generic by resolving these records from signed bundle items instead of compiling the workflow layer into the core dispatch path.

Isolation remains node authority rather than item composition. See
[Execution Isolation](../node/execution-isolation.md).
