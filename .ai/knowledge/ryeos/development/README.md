<!-- ryeos:signed:2026-09-22T00:31:25Z:7496f8c85964b955aea2cce0145a13cade76f2683312c414c6635deae89fda1b:Ng67p1AcPv8u7fwcARTnU9rGCShgqQkHX6z1o8GPjyA/4HQBQ0JbYHj/Q4svtbWB4kNiQwRZLUyoFL937tCMCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "README"
title: "RyeOS Repository Development Knowledge"
description: "Scope and index for contributor-facing knowledge used to change, test, review, and release the RyeOS repository"
entry_type: reference
version: "1.0.3"
```

# RyeOS Repository Development Knowledge

This directory is for development work on the RyeOS project itself. Its audience
is repository contributors and coding agents modifying this source tree. It is
not the installed RyeOS knowledge base and must not become a holding area for
product architecture, operator activation, or user workflows.

## Placement rule

| Knowledge | Location |
|---|---|
| How to build, test, sign, review, migrate, debug, or release this repository | `.ai/knowledge/ryeos/development/` |
| Implemented RyeOS concepts, runtime contracts, user workflows, and cross-bundle operator runbooks | `bundles/standard/.ai/knowledge/` |
| Knowledge inseparable from a self-contained optional bundle and intentionally available only with that bundle | That feature bundle's `.ai/knowledge/`, when the bundle provides knowledge |
| Explicitly deferred design work | `.ai/knowledge/ryeos/future/` |
| Research arguments and papers | `.ai/knowledge/ryeos/papers/` |

A development entry may state implementation invariants and name owning source
files when that information is needed to change the repository safely. It should
not be the only home of an implemented runtime or operator contract that RyeOS
users need after installation. If both audiences need the subject, keep the
installed contract in bundle knowledge and make the development entry a focused
contributor guide rather than duplicating the product documentation.

## Current contributor set

- `architecture` and `dependency-constitution`: repository layout, ownership,
  and dependency direction.
- `host-capability-boundary`: required ownership and review gates for complete
  host operations, interactive process migration, fixtures and diagnostics.
- `dev-workflow`, `build-and-test`, `remote-development-and-qualification`,
  `ui-development`, `mcp-setup`, and `source-local-bundle-development`:
  contributor workflows.
- `signing`, `release-process`, and `bundle-format-migrations`: publication and
  migration procedures for repository changes.
- `persistence-schema-evolution` and `filesystem-durability`: implementation
  constraints that new storage and recovery code must preserve.
- `chat-latency-investigation`: a development investigation runbook.
- `steering-graph-interrupt-and-cancel-path`: an implementation decision record.
- `admitted-execution-recovery`: a contributor-facing map of the recovery code
  and the invariants changes to that code must preserve.
- `ui-design-system`: the governing Gruvbox visual language, nested-workspace
  direction, component anatomy, contextual-input rules, renderer/content
  ownership and visual review criteria. Read it before implementing or restyling
  web or terminal UI. It preserves the launcher, optional slots and authored
  ambient character; the rejected neutral-palette/sidebar redesign is superseded.

The installed worker-hosted execution contract lives at
`knowledge:ryeos/core/execution/worker-hosted-execution` in Standard.
Provider-specific activation and operator knowledge belongs to the optional
feature bundle that implements that provider integration.
