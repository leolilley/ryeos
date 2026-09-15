<!-- ryeos:signed:2026-09-04T09:10:36Z:f3598a848390f84b43181d8ca8f003aea52f1be0a31b34cd3bb44b9fe1a2a1f8:pj1ciGhfPMtfJMoLzxVL06OIBEQIzLQ24PFUQvJUcCYxTkb+z49xnlIW/8t9iphn5UQytRS9aZK/eag9UqxoDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: project-ai-surface-registry
title: Project `.ai` Deployable Surface Registry
description: Deferred signed registry for project-authored deployable AI surfaces
entry_type: design
version: "1.1.0"
```

# Project `.ai` Deployable Surface Registry

## Status

Deferred refinement. The June 2026 implementation landed the concrete sync and
schedule reconciliation path; this note captures the remaining architecture
cleanup.

Already landed:

- typed project sync surfaces in `ryeos-state`;
- explicit `file` and `directory` surface shapes, including the exact root
  `.ai/manifest.source.yaml` and `.ai/manifest.yaml` files;
- `.ai/config/schedules`, `.ai/graphs`, `.ai/config/execution`, and
  `.ai/config/ryeos-runtime` as deployable project `.ai` surfaces;
- the generic `.ai/config/development` namespace for source-local project
  development intent, with project identity expressed below that surface;
- `remote sync-project-ai` copying managed project `.ai` content;
- project schedule declarations reconciled into node-owned schedule specs under
  `<system_space>/.ai/node/schedules`;
- ownership/conflict checks for manual schedules and schedules managed by other
  projects.

## Deferred work

The deployable surface list and each surface's shape are still encoded as Rust
data. That is acceptable for the current implementation, but RyeOS should
eventually expose a signed, discoverable registry for deployable `.ai`
surfaces.

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
