<!-- ryeos:signed:2026-08-13T03:35:01Z:7a76c8d1ddf933aa97044ec444e036b3393b20ff88d1cc807723100ded9182e5:ELo9Qmc5CZ1Uk1diYYO6gUMZz7KrjJdcYr4vAzipxwAQGnCWXm3fuZuPSveyxjfskBCc5ADdu2duJF95li9cAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/kinds
tags: [kind, worker, persistent-session, source]
version: "1.0.0"
description: Worker kind and adjacent-source reference.
---

# Kind: worker

A `worker` is a publisher-authored persistent subprocess definition. The kind
owns the mechanical lifecycle, protocol, target-substrate, effect ceiling, and
adjacent-source ceiling; workloads remain ordinary authored data.

- Directory: `workers/`
- Composer: identity
- Source declaration: required root-verbatim `{root, entry, digest}`
- Source testimony: descriptor owner signs the aggregate source-manifest digest
- Protocol: kind-declared persistent session

Worker source is co-located under the worker owner's namespace. For an item
`worker:<owner>/<name>`, a declaration such as `root: lib/<name>` addresses
`.ai/workers/<owner>/lib/<name>/`, and `entry` is relative to that root.
`${source.entry}` is a typed complete argument; it
cannot be interpolated into command, environment, working-directory, or a
larger argument string.

RyeOS captures the regular-file-only source tree from the exact publisher
generation, compares its canonical manifest with the signed digest, and
retains a separate authority binding. Recovery reopens only retained CAS
content and rechecks current publisher/kind trust. External runtimes, models,
datasets, toolchains, and other opaque dependencies remain separate
`external_content` declarations.
