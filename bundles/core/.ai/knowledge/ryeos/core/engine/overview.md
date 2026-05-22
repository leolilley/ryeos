<!-- ryeos:signed:2026-05-22T04:30:07Z:ab59733e2d5a16901543bdb5d86201bfabc68afc67c4f4d7f7c36c07cb86fe5b:oUjWjxqKO0lLY9zKrzB3c28dC+pSyVtUsRL0/Ua0JJdvi2oAdpgjyPfbLuVaXmNoNoxCtsRbW0TUEdc4by7SAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/engine
tags: [engine, architecture, parse, compose, execute]
version: "1.0.0"
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

Core owns the generic machine: config, handler, parser, protocol, runtime, service, node, tool, and streaming_tool kinds. Standard contributes workflow kinds and runtimes such as directive, graph, and knowledge.

## Extension points

- **Kind schemas** define directory mapping, accepted formats, composer, metadata extraction, and execution model.
- **Parsers** bind a format to a handler binary and parser config.
- **Handlers** are parser/composer binaries loaded during bootstrap.
- **Protocols** define subprocess wire contracts.
- **Runtimes** are runtime binaries serving delegated workflow kinds.
- **Services** are in-process daemon endpoints.

The engine stays generic by resolving these records from signed bundle items instead of compiling the workflow layer into the core dispatch path.
