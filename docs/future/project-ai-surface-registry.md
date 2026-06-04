# Project `.ai` deployable surface registry

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
