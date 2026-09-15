<!-- ryeos:signed:2026-09-09T20:42:50Z:cbe2c9c58540bd1241d47b9861f23451d8b24473bc3564b72b268045da7c9bad:JjGPOTHVu3emdSN8kwCYU3CcLB1dVWR6q/1pKa1DGEkrZdg6sIFnbkdB/b0g2P+odf7w/rCeczfzPFX5EohoBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/kinds
tags: [kind, worker, persistent-session, source]
version: "1.0.2"
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

Authored source location is not a required directory in the work project.
Under enforced isolation, a persistent session receives its retained source at
`/ryeos/realizations/source-closures/<source-binding-hash>/`; `${source.entry}`
resolves there. This uses the existing read-only execution-runtime authority,
with the exact CAS materialization and descriptor lease. It creates no project
mountpoints or candidate exclusions. Direct Tool source loaders retain their
existing admitted project-namespace mapping; they are not implicitly relocated.
Disabled session execution retains its bounded private runtime-view delivery,
not a host `/ryeos` directory or an isolation fallback.

Persistent-session capsule schema 11 makes this launch-coordinate change an
explicit clean cut. Predecessor capsules cannot resume under the new placement
semantics; source-binding identity and schema are unchanged.

Worker content and selected environment content can use either `project` or
`execution_runtime` mounts. The latter uses the existing fixed
`/ryeos/realizations/<mount>` namespace and requires enforced isolation; it is
never copied into the project as a substitute. Project-root execution remains
available with disabled isolation under its admitted private-workspace policy.

For structured-session profiles, `workload_executable` is an exact canonical
relative member of the selected workload realization, such as `bin/program`
for a Tree. A File realization requires its exact single filename. RyeOS does
not search PATH, flatten the product layout, or follow executable symlinks.

Executable search, workload executables and environment-file paths resolve from
the retained realization's own mount root. Their descriptor handles retain the
admitted files and adjacent resources; ambient PATH or host libraries do not
repair missing content. Runtime closure qualification additionally requires the
`captured_execution` filesystem ceiling and the workload's own command-sandbox
permissions for the admitted paths. A worker may select that ceiling through
its kind-owned `filesystem_authority` projection; omission preserves
`node_policy`. The selected ceiling narrows the ordinary prepared execution
plan and is retained with it. It does not independently enable isolation.

Environment contributions obey both the source-kind/runtime policy and the
receiving worker kind's content ceiling. The worker's authored content plus all
selected content contributions are checked together for roots, collisions,
count and storage grants. These mechanical ceilings are retained for recovery;
the environment remains the consumer-binding owner. Evidence attachments retain
their separate admission policy.

`session_resources` is a closed, kind-admitted override mapping. The current
contract permits a signed worker to request a `real_uid_process_limit` no
higher than the kind-owned ceiling; absence retains the kind default, unknown
fields fail admission, and the effective value is capsule-bound. Because
`RLIMIT_NPROC` is real-UID scoped, it is a finite shared-host ceiling rather
than per-worker containment.

See `knowledge:ryeos/core/execution/worker-hosted-execution` for the
session-bound hosted-execution lifecycle built on this kind.
