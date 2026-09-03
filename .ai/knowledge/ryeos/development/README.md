<!-- ryeos:signed:2026-09-01T21:21:33Z:fe8cf363e0d6716fe435faf4847bedd1a35254e7eea3f61e0627a5770057c52a:H9kGHVMne2ipv7s/qMQomi4bhW/c8PIE1Gq4lrzrfCUw1sNhDsj1WmkPZM0nSE/4/WvlilHFAbN41ugbPgwBDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "README"
title: "RyeOS Repository Development Knowledge"
description: "Scope and index for contributor-facing knowledge used to change, test, review, and release the RyeOS repository"
entry_type: reference
version: "1.0.1"
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

The installed worker-hosted execution contract lives at
`knowledge:ryeos/core/execution/worker-hosted-execution` in Standard.
Provider-specific activation and operator knowledge belongs to the optional
feature bundle that implements that provider integration.
