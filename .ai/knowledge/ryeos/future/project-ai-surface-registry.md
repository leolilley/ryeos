<!-- ryeos:signed:2026-07-21T00:24:56Z:086427b6f697daa14f51fc072d4691382754004ca2258052c6582fdc2786807b:qYvfM3mdVdD9fIpMStdtB9U5ZmogZkEbh2yPfcxT+Skuu4KDBbhM3E+T3GmPBfwd8heBI8IAW3h1jtoJziiLCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: project-ai-surface-registry
title: Project `.ai` Deployable Surface Registry
description: Deferred signed registry for project-authored deployable AI surfaces
entry_type: design
version: "1.0.0"
```

# Project `.ai` Deployable Surface Registry

## Status

Deferred refinement. The June 2026 implementation landed the concrete sync and
schedule reconciliation path; this note captures the remaining architecture
cleanup.

Already landed:

- typed project sync surfaces in `ryeos-state`;
- `.ai/config/schedules`, `.ai/graphs`, `.ai/config/execution`, and
  `.ai/config/ryeos-runtime` as deployable project `.ai` surfaces;
- `remote sync-project-ai` copying managed project `.ai` content;
- project schedule declarations reconciled into node-owned schedule specs under
  `<system_space>/.ai/node/schedules`;
- ownership/conflict checks for manual schedules and schedules managed by other
  projects.

## Deferred work

The deployable surface list is still encoded as Rust data. That is acceptable
for the current implementation, but RyeOS should eventually expose a signed,
discoverable registry for deployable `.ai` surfaces.

Goals:

1. Keep broad `.ai/node` sync forbidden by default.
2. Let bundles/platform config declare new deployable surface roots with
   ownership and reconciliation metadata.
3. Preserve fail-closed behavior when a surface is unknown.
4. Keep project-authored intent separate from node-owned runtime projections.
5. Surface better diagnostics when project `.ai` content is ignored because no
   deployable surface is registered.

Do not implement this until at least one more deployable surface needs a custom
reconciler; the current static registry is sufficient for schedules and current
project config surfaces.
