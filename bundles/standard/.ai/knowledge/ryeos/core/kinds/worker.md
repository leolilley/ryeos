<!-- ryeos:signed:2026-08-11T02:28:31Z:6ac4cddf68564f333d1a316f6c358f67afc6a264b4142efe7e926d5527c5732e:m+rtJdR/vcXMsRraugu3FfX/WpsdgQrklq7WI1DInRst5Gvims8iIwnt1Z7S9ndhWXC/5iCxhhokN8xSE74gAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

Worker source is co-located under the worker owner's namespace. For
`worker:standard/local-tinygrad`, the declaration
`root: lib/local-tinygrad` addresses
`.ai/workers/standard/lib/local-tinygrad/`, and `entry: bootstrap.py` is
relative to that root. `${source.entry}` is a typed complete argument; it
cannot be interpolated into command, environment, working-directory, or a
larger argument string.

RyeOS captures the regular-file-only source tree from the exact publisher
generation, compares its canonical manifest with the signed digest, and
retains a separate authority binding. Recovery reopens only retained CAS
content and rechecks current publisher/kind trust. External runtimes, models,
datasets, toolchains, and other opaque dependencies remain separate
`external_content` declarations.
