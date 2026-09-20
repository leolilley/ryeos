<!-- ryeos:signed:2026-09-20T01:35:20Z:ae0ac0da5051f4c8516874eb9b9acbd3c602defa99250f67e5a1cdab7e8a6965:pSmXL3n0bMmMGnfzcI8+yNGxOt2DvTed/elF5Xn+NH8UjiQs2tpqg5bpOro1DhvYV/Tysr08Jr+Q3AuqGCR9Ag==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "ui-development"
title: "Browser UI Development Workflow"
description: "How to develop and qualify the typed Svelte RyeOS browser renderer"
entry_type: reference
version: "3.0.0"
```

# Browser UI Development Workflow

Read `ui-design-system.md` first. Preserve the Gruvbox palette, tiled view sets,
launcher, optional slots, authored ambient character and shared Rust-owned UI
semantics. Do not turn RyeOS into a generic sidebar application.

## Ownership

### Admitted binding attachments

The browser session authenticates the principal; it does not supply one mutable
project binding for every view. Each mounted view retains an admitted binding
attachment. Requests carry its exact id, generation and digest. The daemon
resolves that triple inside the authenticated session and obtains project
authority from its retained pinned directory, never from a displayed path.

Resolve definitions by mounted instance → attachment → view reference. Never
merge same-named views from different attachments into one global view map.
Moving a view, changing focus or opening another project cannot rebind existing
views or their composers. A view set's insertion attachment is only the explicit
context for adding a new view; it is not a fallback for mounted operations.
Lens return frames retain their original binding as well as selection state.

The immutable `surface_attachment_id` owns authored shell sources and initial
composition. It is not a default dispatch authority. Project-open adds a newly
admitted attachment and composition, rather than replacing the browser session.
Late results must still match their original attachment and mounted view before
updating presentation. Current policy and attachment admission are also checked
by the daemon; client checks do not grant authority.

Attachment count is bounded by the signed `ui_browser_sessions` node policy.
There is no daemon fallback value. Test fixtures must supply that policy too.
Releasing an unused attachment is an explicit session-wide operation: another
browser tab may share it. Never infer permission to revoke from one renderer's
last closed mount. The shared command overlay offers release only when this
renderer has no mount, insertion context, return frame or pending request using
the exact attachment. The daemon settles only its UI seat leases before revoking
the binding; it does not terminate project executions. The original surface
attachment is retained until the session itself ends. A failed release remains
visible and requires an explicit retry, not a background mutation loop.
Reusable arrangements carry no live binding triples. Multi-context resume must
re-admit owner-specific references before restoring mounts. Particular-set
resume does this through stable project-registry and logical-work identities;
ordinary layout-preference export still refuses mixed attachment contexts
rather than silently restoring them under the surface attachment.

### Saved view-set library

The signed `view:ryeos/view-sets/library` uses the existing principal config
get/update services. It is not a browser storage feature or a second personal
state service. Shared Rust captures and validates the active composition;
`save_active_view_set` invokes its declared signed companion through the normal
compiled binding path. The daemon still validates and revision-fences the write.
Authored companion presence never establishes admission.

Saving uses the active set's name. A duplicate name is refused, not overwritten:
rename the open set to save another composition. Concurrent library changes
produce a conflict; refresh re-observes current data without retrying the write.
Updating/deleting a named saved composition is not implemented by this flow.
Capture omits the exact invoking management view from a detached composition
copy; the live layout and other instances of the same view are unchanged.
The durable library size ceiling is not a transport allowance: saving checks
the complete binding request against the current compiled session bounds before
dispatch. Do not enlarge route limits or guess envelope overhead in the UI.

`open_saved_view_set` is a local composition operation. It creates fresh mounted
identities and revalidates against the invoking view's admitted attachment. It does not
restore credentials, grants, execution state or input drafts. A saved template
and a resumed UI session are different contracts.

Reusable templates encode selection relationships by stable mount address.
Own-set followers remain own-set followers. Cross-set followers name another
saved template and reopen only when that template has an explicit live set
resolution. Pins become named required fresh-subject inputs containing only
logical facet names: neither captured values nor fingerprints are durable.
Every mounted tile and slot must have exactly one relationship; omission is not
an implicit follow-own-set default. Missing linked sets refuse before the
composition mutates. A default reusable-library open does not borrow the
invoking mount's ambient selection. A required subject with no explicit fresh
input mounts visibly unresolved while the rest of the composition opens. That
mount is an engine-wide execution fence: eager, on-demand, mention and
completion reads, UI/service/Rye affordances, composers and input submission
all refuse until completion. Rust projects its exact input/facet requirement
and compatible source-set choices to every renderer. Activation re-resolves
the named set's current scoped facets, revalidates the target binding and byte
bounds, and installs an exact pin without accepting client-provided values.
Saving or duplicating an unresolved mount preserves only that relationship
shape. Explicit upfront subject supply and particular-set resume remain strict:
missing or partial inputs fail atomically and never degrade to unresolved.

Particular view sets have a separate revisioned principal-config collection.
They retain reusable composition plus only a registered project `local_id` and
logical work `chain_root_id`. Resume reopens current project authority, compiles
a fresh binding attachment, verifies every continuation placement against the
same chain root, principal and project, resolves the current head, and then
mounts the composition. The logical-work coordinate can freshly supply only
`selection.work`, `selection.work.thread`, and `selection.work.chain_root`;
other pinned subjects require a future stable locator and must not be retained
as a particular set. It never restores
paths, placement thread ids, grants, drafts, observations, credentials or
execution state, and it never starts or restarts work. Missing, denied or
changed references are explicit failures. Projectless records can resume only
from the exact projectless invoking attachment. A newly published project
attachment that cannot be mounted must be released through its exact retained
coordinate; removing only browser-local state leaks daemon authority.

The library remains useful in observation-only sessions, but persistence can
refuse when its companion coordinate is not admitted. Never broaden grants to
make a visible control work, and never infer per-coordinate admission from
authored definitions. Availability presentation is not an authority check.

View-level `refresh.after_invoke` is an explicit boolean. It re-observes mounted
view sources after invocation settlement, including refusal. It is not a
source-level trigger and must not retry mutations or recreate closed views.
Settlement uses the retained invocation mount, not whichever view set is active
when the response arrives. This coordinate is separate from an input origin.

Selection attachments are shared Rust runtime state keyed by mounted instance.
Following names a retained open view set; pinning captures bounded declared
selection values, including unresolved absence, from the current attachment.
Neither is project admission or a capability. Moves preserve that attachment;
following a different set is an explicit operation, never a placement side effect.
Pinned views refuse selection writes. A followed set cannot close while external
mounted followers still depend on it. Accepted closes retire owned sources and
attachments; explicit reattachment fences earlier responses before refetching.
Reusable templates exclude pinned values and runtime view-set identities while
preserving their portable relationship grammar as described above. Ordinary
layout-preference export remains presentation-local and refuses relationships
or mixed project contexts it cannot represent. Do not weaken that refusal just
because the named saved-set library has a richer durable contract.
Open alongside allocates a fresh mounted viewer, captures its origin's current
selection, and installs that pin before resolving source requests. It does not
copy drafts, pending effects or execution authority. Selection-independent views
must not acquire fake follow relationships merely because they moved. Writers
and subscribers can follow another set even when they have nothing to pin.

Selection and composer route are separate owners. A conversation's input.route
remains instance-scoped even when selection is pinned or rebound. A signed
cross-view Talk action must explicitly open its destination before assigning
that destination's route. Without an opened destination, a route write belongs
to its originating mount, never whichever control has keyboard focus.

The browser is a renderer, not a second application model:

- `crates/clients/base/src/ui/` owns semantic state, layout, events and effects.
- `crates/clients/web/src/wasm.rs` is the typed Rust/WASM boundary.
- `crates/clients/web/browser/generated/` is generated from the Rust contract.
- `crates/clients/web/browser/` owns Svelte rendering and browser adapters.
- `crates/clients/web/pkg/` is generated, reviewed installed output. Never edit
  it directly.
- `crates/daemon/ryeos-ui/web-assets.json` is the closed asset manifest.
- `crates/daemon/ryeos-ui/src/assets.rs` serves only that registered closure.

The installed closure is exactly `index.html`, `ryeos_ui.js`, `ryeos_ui.css`,
`ryeos_three.js`, `ryeos_web.js` and `ryeos_web_bg.wasm`. There is no alternate
browser renderer, compatibility bootstrap or runtime fallback.

Svelte owns component lifecycle and presentation. Rust remains the sole owner
of work, authority, navigation, layout and effect semantics. Browser-local
state is limited to mechanical focus, selection, scroll restoration, pending
transport handles and opaque Rust-issued layout preference bytes.

System chrome shows the authenticated principal and signed surface/session
context, not the launch project's path as if every mounted view shared it.
Project exceptions belong on the exact mounted view attachment. The bottom
status line is quiet interaction chrome: relevant shortcuts and exceptional or
transient connection/save feedback are appropriate; permanent tile, item,
thread, token, principal, surface, project and debug inventories are not.

Pointer layout controls consume exact guarded Rust intents. Master promotion is
available only for a non-master tile in an authored master-and-stack set and
names that mounted tile; it is not maximise/restore or a misleading swap action
on the current master. Group-tab ordering uses the same canonical layout tree
and cannot be inferred from DOM order. Keyboard focus has a non-colour
structural cue even when tile chrome is hidden or the mount occupies an edge
slot, and remains distinct from selected content or running state.

At narrow widths the authored navigation remains reachable through a
renderer-local drawer. Open/closed state, focus trapping and Escape dismissal
are browser mechanics only; destinations and activation intents remain the
Rust projection. Dismissal restores the opener only while the drawer still owns
focus; an activated destination that took focus keeps it. Never hide an authored
navigation region with no accessible replacement or introduce a browser route
model to solve responsive layout.

Execution views must project coverage as data. Bounded child trees declare
complete versus truncated closure, candidate views distinguish not applicable,
not yet captured and retained, and durable execution posture distinguishes an
available audit record from missing, unreadable or corrupt evidence. An empty
section must not silently stand for all of these states. A retained node-signed
audit document is available only after exact current-node signer, content hash
and signature verification; parsing an envelope-stripped body is not evidence.

## Focused source loop

Edit authored files under `crates/clients/web/browser/`, then run:

```bash
cd crates/clients/web
npm run check:renderer
npm run build:renderer
```

The build writes a private generation to `target/ui-browser-stage`; it does not
publish `pkg/`. The stage validator rejects extra chunks, source maps, bare
imports, external URLs and any lazy chunk other than `ryeos_three.js`.

Use the local asset proxy or daemon override only with one complete staged or
published generation. Never combine staged JavaScript with embedded WASM from
another generation.

```bash
scripts/dev-ui-assets.sh --background --open
```

The proxy obtains a normal one-shot browser launch from `ryeos web`. Do not
open its `/ui` route directly unless that origin already owns a valid session.

## Required boundary checks

```bash
cargo run -q -p ryeos-ui-contract-exporter -- --check
cd crates/clients/web
npm run check:renderer
npm run build:renderer
npm test
npm run test:wasm-contract
```

The WASM test is not replaceable by a JSON fixture: it proves actual serde /
wasm-bindgen enum, optional and `u64`/BigInt behavior. The mounted-root browser
test additionally requires the exact Playwright browser revision from the
declared development environment; never substitute a host browser silently.

## Ambient scene

`scene_model.rs` emits semantic scene objects. `AmbientLayer.svelte` retains one
`ryeos_ambient_scene.js` controller and updates it from accepted envelopes.
Three.js is the exact local `0.128.0` build chunk; there is no CDN import.
Visibility comes from the authored surface projection, never a browser-side
view-name exception. Animation state does not become RyeOS semantic state.

## Scene, atlas and field views

Map and Atlas are ordinary views placed by the shared Rust-owned layout tree.
A compact execution map may occupy a supporting tile while the same view can be
promoted into the central region; Atlas usually benefits from a larger region
with selection or evidence in adjacent tiles. These are authored
arrangements, not fixed browser positions.

`SceneView.svelte` receives only the projected scene and owning tile identity.
It must not accept a browser-authored map/atlas kind, rendering mode or visual
style. A scene containing the typed atlas projection renders its projected
atlas controls and interactions; another scene renders its generic semantic
objects. Rust remains authoritative for which model exists and where it is
placed.

Field is likewise a normal typed view. Svelte owns its toolbar, event rail,
details, accessibility tree and preview composition. The retained canvas
controller owns only bounded drawing, hit testing and pointer mechanics. Never
restore a hidden DOM renderer, compatibility mount function or browser-side
application model behind the Svelte component.

Rows, tables, sections, timelines and field selections must render the bounded
detail records already projected by Rust. Pointer disclosure names the exact
mounted instance and stable item id; it must not synthesize a select-then-expand
sequence against whichever row later has focus. Detail rendering is semantic
text (`dl`/`dt`/`dd`) and never interprets evidence into browser-owned status.
Dock mounts use the same exact disclosure event and Rust-owned expansion state
as center mounts. Tables retain native button semantics inside explicit ARIA
rows and cells rather than assigning row semantics to a button.

## Rebuild boundaries

- Svelte/CSS changes require browser stage rebuild and complete asset
  publication.
- Rust UI/WASM changes also require contract export and WASM regeneration.
- Asset registry or route changes require the focused `ryeos-ui` rebuild/tests.
- Bundle definition changes require the normal signed bundle publication path.

Do not invoke Node, npm, Vite or contract generation from Cargo `build.rs`.
Ordinary Cargo builds only embed and validate the checked-in asset generation.

## Common mistakes

- editing `pkg/` instead of authored browser source;
- adding browser-side semantic stores, routing or authority decisions;
- bypassing the single FIFO reducer/effect commit path;
- importing daemon endpoints from visual components;
- restoring predecessor module names or fallback logic;
- using an incomplete asset directory with `RYEOS_UI_ASSET_DIR`;
- accepting a build produced by ambient host tool versions as deterministic
  release evidence.
