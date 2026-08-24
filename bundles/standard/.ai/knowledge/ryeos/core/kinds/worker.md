<!-- ryeos:signed:2026-08-24T03:14:21Z:0e2ee8b110b00fa7a044c6e40bc1026164579bb7765438726d3a1e302399859b:6LTYsj4ZMobi0TralVTSUF3dw69UbGuOWiB8CXGAVTQRzURYrU/+Z6aK+bE9aT1usr/BqJlDK4cVS47dlsbGDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/kinds
tags: [kind, worker, persistent-session, source]
version: "1.0.1"
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

`session_resources` is a closed, kind-admitted override mapping. The current
contract permits a signed worker to request a `real_uid_process_limit` no
higher than the kind-owned ceiling; absence retains the kind default, unknown
fields fail admission, and the effective value is capsule-bound. Because
`RLIMIT_NPROC` is real-UID scoped, it is a finite shared-host ceiling rather
than per-worker containment.

See `knowledge:ryeos/core/execution/worker-hosted-execution` for the
session-bound hosted-execution lifecycle built on this kind.
