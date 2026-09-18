# Session project authority implementation: F06–F07

Status: retained-handle, stable-query and typed engine-integration implementation complete; direct and compiled browser-dispatch integration pass. Parent plan: [README](README.md).

## Implemented result — 2026-09-18

`RetainedProjectAccess` carries the pinned directory owner and its separately validated stable query identity. Filesystem consumers keep the owner through the operation; thread/work/field projections use the stable identity and retain principal/project filters. All UI handler callers were migrated together.

The direct two-project integration covers equivalent-path/symlink selection and opposite-project substitution. The stronger compiled-session dispatch test exposed an additional mint-time defect: ordinary engine resolution rejected `/proc/self/fd/N` before handler dispatch. The correction uses typed `AuthoritativeProjectContent` on the retained `PinnedDirectory` for trust/parser overlays, surface/view composition and target resolution. It does not canonicalize/reopen the ambient path and does not globally weaken no-follow traversal. A replacement-path negative test proves reads remain on the original pinned object. The populated exact compiled-dispatch journey passes and returns only the selected project's thread.

## Two different representations, two different purposes

The current helper conflates filesystem access and projection lookup. Replace that ambiguity with explicit interfaces:

| Purpose | Representation | Lifetime / permission rule |
|---|---|---|
| Filesystem resolution/traversal | Retained `PinnedDirectory` or owner-bearing wrapper | Owner stays alive through every dependent operation; descriptor-relative access |
| Thread/work projection query | Validated stable project identity matching the stored schema | Derived from retained session authority after path-binding validation; never reopened to grant filesystem access |
| Human display | Display path/label | Informational only |

Prefer existing stable canonical identity for the present `project_root` schema rather than introducing a broad storage migration in this fix. Do not persist `/proc/self/fd/N`; it is process-local and ephemeral. If a new stable ID is desired later, specify a separate projection migration and rebuild contract.

## Files and caller inventory

`crates/daemon/ryeos-ui/src/seat_auth.rs`; handlers `ui_items.rs`, `ui_work.rs`, `ui_threads.rs`, `ui_field_runs.rs`; `thread_authorization.rs`; session mint/project-authority owners; app lifecycle filtering; state `queries.rs`.

Search every call to `project_path`, `project_directory`, `descriptor_path`, and project identity hashing in the UI backend. Include both direct SQL filters and post-query lineage comparisons. Item inspection and list resolution are filesystem consumers; work/attention/history and field-run filters are identity consumers. Some flows need both and must retain both explicitly.

## F06 implementation

1. Add a regression that obtains authority through the session helper, returns from it, then accesses the intended project while churn reuses unrelated FD numbers.
2. Remove the temporary-clone-to-bare-path pattern. Prefer returning the owned directory handle. For APIs requiring a pathname, derive it from a local retained owner whose scope encloses all synchronous/asynchronous work. Do not expose a seemingly durable owned `PathBuf` whose descriptor is already closed.
3. If a callback helper is selected instead, ensure an async future cannot outlive the owner; compile-time ownership should express the constraint. An owner-bearing wrapper is often clearer than manually documented lifetime obligations.
4. Preserve `ensure_path_binding` at the authority boundary. Continue exact descriptor traversal rather than canonicalizing and reopening an ambient pathname.
5. Migrate all callers atomically; do not retain the unsafe helper as a convenience alias.

## F07 implementation

1. Add a separately named query-identity accessor. It validates the retained binding and returns the stable path/identity used by thread persistence, not its descriptor access path.
2. Feed that identity into `ThreadListFilter`, work continuation checks and field subject identity construction. Ensure independently opened handles for the same project produce the same field identity.
3. Keep principal/owner filters and exact-thread authorization. A zero-match bug must not be “fixed” by dropping the project filter or broadening to all node work.
4. Define projectless-session and signed-operator behavior using the existing caller contract. Do not discover the daemon app root or another project implicitly. Explicit operator query parameters remain subject to their current authority rules.
5. Ensure no ephemeral descriptor path was persisted by affected flows. Inspect read-only first. If contaminated projection rows exist, rebuild from authoritative records or provide a separately reviewed repair; never guess a historical canonical path from a now-reused FD number.

## Tests and acceptance

- Returned owner remains alive through item listing/inspection, including async yield and FD churn; no access after owner drop is expected or used.
- Rename/replacement/symlink substitution refuses or remains pinned according to the existing authority contract; never switches to replacement content.
- Two projects with matching thread names: current-project work, approvals/history and field runs include only the correct permitted records.
- Different principals: existing ownership/project access rules hold; unauthorized exact reads remain indistinguishable from missing subjects.
- A continuation chain is queried with stable identity; auxiliary threads cannot admit an unauthorized root.
- Reopening the same session project using a different FD keeps the same projection/field identity.
- Projectless and operator lanes retain documented behavior, with no inferred project.
- Integration: minted compiled browser session → signed source coordinate → handler → nonempty canonical project rows; then replacement refusal. A direct helper test alone cannot close F07.

Close F06 and F07 separately in evidence even if fixed in one commit. No node-policy or wire grant broadening is required or permitted by this plan.
