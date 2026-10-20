<!-- ryeos:signed:2026-10-19T22:29:39Z:4c4167248107e59a4afd74a7e83f73f1899df7cbd5c018423713cf3abb1ee8a4:XaA1NQEbvTTvPBTEtb02m1p0lnCshNyHnSu6wYM7Q/8QKWrhByMsRiWvCIK/MlZ/LdddNKQoMhTa9tgJtn7sCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

### Shared presentation, surface instances, tabs and compositions

Agreed presentation hierarchy (2026-10-03): an authenticated browser session
owns shared presentation and open Surface instances. Each Surface instance has
top-level tabs → one live view-set instance per tab →
nested layout of view instances and view groups. A group's member tabs are
local view selectors, not surface instance-level view-set tabs.

The shared presentation is authored through a typed signed config item using
the existing config kind. "Session shell" is an implementation responsibility,
not a new kind or operator identity. It declares shared dialog View references,
four optional dock mounts and initial Surface references. Resolve its selected
default through existing launch/config owners and compile/admit its View closure
through the existing binding compiler. A signature or installed bundle does not
grant every referenced operation. No hidden Surface or union of open-Surface
grants substitutes for shared config admission.

Launcher, shortcuts and eventual node/settings dialogs remain ordinary Views,
available before a Surface opens. Surfaces may add scoped contextual dialog
Views. Shared and contextual entries retain their exact admitted mount identities;
equal labels/refs do not merge bindings. New bundles do not silently inject UI.

Opening and composition decision (2026-10-08): the generic `ryeos-ui` presentation
opens neutral Home/background with the shared launcher. It is not a mandatory
node dashboard or maintenance console. Purpose-specific bundles can author a
shared presentation config, Surfaces and Views, arranging ViewSets through the
existing composition mechanisms; this introduces no new ViewSet kind. An explicitly selected presentation chooses initial
Surfaces; an explicitly opened Surface supplies its tab seeds. Bundle installation
does not replace a user's selected presentation, append tabs or confer grants.

Neutral Home has no eager domain polling, conversation input or View library;
its authored background is sufficient. Chrome derives connection state from
transport and identity context from the authenticated BrowserSession, not from
an optional Node status source or an invented health assertion.

The shared Surfaces catalogue is an ordinary View over bounded content discovery
(`items/list`, kind `surface`), with coverage visible. It is not an admitted-to-open
list. Search uses the View's ordinary `input.feeds` query parameter on the same
source, preserving its authored kind and limit. Do not filter only the first page
in the browser or imply exhaustive enumeration. Explicit selection opens the exact canonical ref through fresh Surface
admission in the retained shared root's context, not the focused tab's project.
The existing acknowledged preparation owner supplies the creation request ID.
Catalogue filters use the existing typed general item spaces, `project` and
`bundle`. Unknown values and node launch-config scope are bad requests, not empty
observations. An explicitly unregistered kind reports unavailable coverage with
`kind_not_registered`; a registered kind with no matching items is observed-empty.
These facts come from the existing resolution owner, not a new availability
registry. The launcher library stays selected-composition-local: embedded closure entries
do not imply a global menu of bundle Views. Do not add an automatic bundle
contribution registry or inject tabs during discovery.

Shared open/register/save/resume actions require their signed operation
capabilities and descriptor minima as well as existing exact session, attachment,
producer and seat checks. Config requests are not grants. Named request entries
alone do not enforce exact-target restriction when a target declares no required
capabilities. Missing or denied optional targets must stay distinguishable from
successful empty observations.

Access follows existing admission for the acting identity, node and subject,
including the exact compiled attachment and operation capability. Do not add
public/operator UI modes, a universal operator-only gate or renderer-side access
inference. Each source must honour its actual read scope; each action must pass
its own current admission. Node-wide observations are not principal-filtered
activity, and existing source contracts must state that distinction. Maintenance
actions belong in explicitly chosen specialised arrangements. No diagnostic action
is required to make the neutral generic presentation usable.

Keep true absence separate from failed resolution. Item inspection maps an
authoritative not-found result to absence, but unavailable filesystem/content
resolution to a sanitized failure; do not expose private paths or erase failure
as an empty catalogue. Browser cookies alone do not authorize domain reads.
Fixtures that need session-associated source authority must use the existing
exact compiled dispatch, not weaken the admission owner to accept a unit wrapper.
Validate independent source channels independently: a failed authenticated event
observation invalidates its accumulated events, not a separately verified pulse.

The top/left/right/bottom docks belong to shared presentation, outside any tab.
Their visibility and temporary focus can change without detaching mounted Views.
In one document, Surface/tab changes do not recreate shared mounts. Additional
documents require explicit mount/seat coordinates and producer ownership; they
cannot share one mutable producer. Review that ownership before implementing
cross-document shared state. Dock position never selects another View's target.

The source composition owner keeps shared mounts outside Surface tab layouts.
Dialogs render ordinary admitted Views; each input has an exact mount address.
Contracts and saved compositions must describe that same hierarchy. Source
changes are not evidence of signed publication or installed acceptance: those
gates are recorded separately in the implementation ledger.

Immediate scope is the reusable UI foundation and platform-aligned inspection
through ordinary retained sources and admitted operations. Node operations and
later ARC/application arrangements compose those same Views. Conversation history sharing,
automatic context following, global assistant orchestration and provider chat
integration are deferred. Keep existing composers explicitly bound. Shared dock
availability does not decide whether a future conversation follows a Surface or
retains one subject across Surfaces.

A tab is the presentation container, not another authored kind. Its view set
is the mutable composition. Opening a single view may create a one-view set;
adding/removing/moving views edits that live composition. Saving a reusable
template is explicit and does not automatically overwrite its authored seed.
Particular resume retains subjects only through existing revalidation rules.
Closing presentation does not terminate the underlying execution.

A Surface definition is an authored collection of initial view-set tabs and
presentation/binding declarations. Open creates one live surface instance;
opening the same definition twice creates independent presentation state.
Client launches the authenticated session; session owns surface membership,
admitted attachments and presentation seat membership. One independently
presented Rust core uses one seat/producer for shared mounts and zero or more
Surface compositions. Closing the last Surface keeps shared controls usable.
Surface instances
own tabs and compositions, not authentication or seat authority. No separate
canonical Window owner. Browser window/tab placement is adapter mechanics.
Home, Project development and ARC solve can be distinct Surface definitions
opened in ONE session. Each view retains its own admitted binding.

Intended opening defaults: a view opens into the current view set with explicit
placement; one view set opens as a new tab in the current surface instance; an authored
Surface opens as a new live instance, with importing its sets into the current
instance as an explicit alternative. Surface already owns that collection;
do not add a separate collection kind/catalog or mint a new session. Reuse
current authored owners where they fit;
do not invent a second browser application model. Opening a solve composition
must not start workers, allocate compute or imply accepted evaluation.

Canonical source owners:

- `RyeOsCore.presentation` owns one `PresentationComposition`. It contains
  shared mounts and independent `SurfaceComposition` instances; each Surface
  owns its ordered view sets and selected tab. Do not duplicate that mutable
  layout in another vector or browser store.
- Authored shared Config selects initial Surfaces. `SurfaceSpec.view_sets`
  supplies each Surface's ordered tab seeds. Selection is not admission:
  resolve and compile every Surface's own closure and publish membership only
  after validating the complete operation.
- `SavedViewSetTemplate` is reusable composition with fresh required subjects.
  Particular resume re-resolves retained subjects and its explicit Surface ref
  against the current principal-owned library revision and admission.
- Principal-owned saved Surface records additionally retain mandatory
  `origin: {surface_ref, project?: {local_id}}`. This explicit authored origin
  owns contextual dialogs, background and Surface sources. Individual ordered
  particular-set refs retain their own work/project subjects. Resume revalidates
  origin and every tab under one checked generation and locked config snapshot,
  then atomically admits one Surface with exact per-tab bindings and one receipt.
  Do not select origin from the first tab, copy current attachment authority, or
  implement collection resume as repeated independent single-set admissions.
  Saved work is an immutable admitted seed, not a synthetic seat event. Bootstrap
  must replay before producing writes. Explicit durable selection (including
  null) overrides the seed. Per-tab insertion context uses its exact binding,
  never the collection origin merely because both belong to one Surface.
- `ui_seat::open` has separate preparation and producer-claim actions.
  Preparation alone never claims a producer on behalf of another document.
  Independent presentations have independent seats. Observe and replay do not
  renew a lease; a copied document cannot silently take over another producer.
- Browser document placement is an adapter operation. An `OpenUrl` or copied
  browser coordinate does not establish membership, attachment or draft
  ownership. Local preferences are scoped caches, not authority.
  A cached structural arrangement is an explicit restore candidate, never an
  automatic replacement for admitted or replayed mounted composition. Restore
  requires fresh allocation and retained acknowledgement through the existing
  composition owner. Scope changes and HMR must not allocate new mount identities.

Do not clone session authority, share mutable drafts accidentally, or move a
view by deleting its source before the destination accepts it. Cross-presentation
movement uses retained settlement, dependency retention and completed replay
admission. Reachable source controls alone do not prove build or runtime
qualification, or establish that a production release is safe to enable.
Track that qualification separately from implementation status.
Independent-document opening prepares retained membership first, then offers an
explicit same-origin link with `noopener`. Browser refusal does not cancel the
prepared membership or prove child death. The URL contains only the nonbearer
presentation lookup; session credentials and grants never belong in URLs.
The child authenticates independently and claims its own unclaimed seat.
Generic new presentation uses the admitted Shared Config's authored initial
selections. Opening a selected Surface collection in another document instead
uses signed SurfaceOpening admission with exact NewPresentation placement,
fresh portable capture and a typed prepared-document completion. The parent
must acknowledge that completion before exposing the link; no selected tabs
are published into its own composition as an intermediate step. Config-default
initialization and explicit-selected initialization have distinct typed evidence.
No automatic operation should commandeer the operator's desktop.

Independent-document availability also depends on the adapter's local host
implementation fact. Core defaults it to false; the web adapter explicitly
enables it, while terminal does not. Start and authenticated reproof preserve
the live adapter fact. Both projected availability and direct intent dispatch
must enforce it in addition to admission checks. Never retain it as a seat
proof, library preference or grant. Same-document Surface and tab operations
remain available independently. External URL dispatch does not prove a window
opened; browser testimony must preserve that uncertainty.

Use authored human-facing labels, not raw canonical references, in normal
chrome. Audit existing Surface/View `name`, view-set `title` and generated VMs
before adding display fields. Surface instance title and numbering need explicit scope;
never synthesize a misleading numbered surface instance without an actual surface instance owner.
An absent hosted-principal display does not establish a node named `local`.
Node, principal and each View's execution target remain separate facts. Show
raw identity coordinates in details, not as a substitute for authored titles.

### Ordinary local presentation Views

Launcher and shortcut Views use the closed `local_projection` vocabulary.
Rust projects current presentation metadata and regenerates guarded actions
against the exact admitted mount, captured insertion destination, subject,
route and layout. Browser records are not arbitrary serialized intents.

An explicitly declared `input.local_filter: true` filters that View's local
projection. It cannot also declare source feeds, network suggestions,
submission or thread routing. Filtering and row navigation need no daemon
round-trip. Executable sources and mutations remain ordinary signed bindings.

### Write-ahead presentation admission

Idempotent opening/resume actions retain their original preparation identity
on the existing seat before dispatch. Only an exact captured local-prefix
acknowledgement from the current producer releases dispatch. A wire sequence,
matching event count or sibling receipt is insufficient.

The returned completion is also retained before composition changes. Replay
uses the original operation identity and the existing typed composition owner;
it never equates an empty effect list with acceptance. Unknown outcomes retain
their evidence rather than creating a new request. The session store validates
the exact full completion/append size before publishing an attachment or
membership. No post-publication size failure may strand an admitted Surface.

`SeatLog` remains the full-history owner. Operation indexes reference its
acknowledged records and fold new prefixes incrementally; they do not duplicate
facet history or silently discard duplicate-operation evidence.

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

The immutable `root_attachment_id` records original Config or Surface launch
provenance. The model separates shared config/View admission
from each live Surface instance's admitted composition; original launch attachment
stays provenance only. Shared controls retain their own admitted coordinates.
Neither coordinate is default dispatch authority. Project-open adds a newly
admitted attachment and composition, rather than replacing the browser session.
Late results must still match their original attachment and mounted view before
updating presentation. Current policy and attachment admission are also checked
by the daemon; client checks do not grant authority.

Attachment count is bounded by the signed `ui_browser_sessions` node policy.
There is no daemon fallback value. Test fixtures must supply that policy too.
Releasing an unused attachment is an explicit session-wide operation: another
browser tab may share it. Never infer permission to revoke from one renderer's
last closed mount. The shared command dialog offers release only when this
renderer has no mount, insertion context, return frame or pending request using
the exact attachment. The daemon settles only its UI seat leases before revoking
the binding; it does not terminate project executions. The original root
attachment is retained until the session itself ends. A failed release remains
visible and requires an explicit retry, not a background mutation loop.
Reusable arrangements carry no live binding triples. Multi-context resume must
re-admit owner-specific references before restoring mounts. Particular-set
resume does this through stable project-registry and logical-work identities;
ordinary layout-preference export still refuses mixed attachment contexts
rather than silently restoring them under the original root attachment.

The binding wire contract is an exact cut, not a compatibility envelope.
`UiBindingAttachment` carries `root_ref`, `root_generation` and a typed
`presentation`: either the shared Config declaration or a Surface declaration,
plus a separate resolved View map. Config does not acquire Surface-wide sources
or an input route. Resolution, compilation, sanitisation, retained admission and
revalidation use that actual root kind and engine identity. Capability patterns
use the runtime authorizer's canonical validator; limits use the signed
`ui_browser_sessions` policy. Never derive identity by hashing presentation JSON
or reopen diagnostic source paths to measure content.

Each attachment also has a mandatory typed `subject`: `projectless`,
`unregistered_project`, or `registered_project` with its stable local registry
ID. The daemon projects this from retained pinned project authority; display
paths do not establish that association. Particular capture uses the exact
tab's admitted binding, not the library View's shared Config binding. Refuse
capture from an unregistered project rather than silently saving it as
projectless. Saving an entire Surface retains its explicit authored origin and
optional stable registered-project reference. Re-resolve that origin on resume
for contextual dialogs, background and sources; individual tabs keep their
own subjects. Neither record retains grants or live attachment coordinates.

Source integration is in progress, not installed acceptance. The v10 contract
requires exact retained Surface mount manifests, a presentation-wide allocation
frontier and the commitment to immutable initialization testimony. The existing
seat journal records initialization before a producer can claim; Prepare retry
revalidates the original root and initial closures under one checked generation
and pinned project authority, never rebuilds testimony from mutable live tabs.
Browser and terminal bootstrap replay this testimony before enabling effects.
Zero-Surface Shared Config bootstrap has a source implementation; no fabricated
Surface, input target or writer is inserted.

Policy schema 3 requires `max_session_bootstrap_bytes`. The complete bootstrap
response envelope is checked before initial membership publication, in addition
to original/current seat append bounds and the individual declaration limits.
Generated TypeScript/WASM/assets and affected signatures must be regenerated
together before publication. Source tests and formatting are not compiled or
installed acceptance. Dynamic layout creation/restoration and the single causal
composition fold have source implementations; their compiled and installed
acceptance remains open. Lens return and transfer settlement use the retained
composition and paired settlement owners documented below; their source-review
evidence is tracked in the implementation audit. Immutable initial manifests
alone do not establish those operations or their runtime qualification.

### Retained lens navigation

Single-lens navigation uses typed composition push/pop records in the existing
seat history. Return frames contain the exact admitted binding, view, subject
and per-mount input route—not arbitrary facet keys, grants, effects or draft
text. Prediction and preparation do not publish the replacement; the existing
completion acknowledgement does. A missing or revoked return binding refuses
without consuming its frame. Breadcrumbs are projections of this retained data,
not a second mutable stack in the renderer or ViewSet.

Input drafts are scoped by exact mount, admitted binding identity, view/input
and logical route scope. Returning to a lens reuses its own namespace, never
copies another View's text. Composition settlement retains only namespaces
matching an exact current mount/View/binding or an exact retained return
frame's mount/View/binding. Ordinary replacement that clears the return stack
must not keep removed View namespaces merely because the mount and binding
remain unchanged. Explicitly cleared or malformed retained input routes are
targetless; they never resurrect the authored initial target.
Only an absent route uses the authored initial value. Route freshness uses the
exact source event's presentation-vector ordinal; local append numbers and
retained chain positions must not be compared numerically.

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
identities in the invoking mount's retained insertion destination. The library
mount's binding admits the library interaction; the captured destination tab's
binding owns insertion. A later focus change cannot redirect that insertion,
and a closed destination refuses before journal mutation. It does not
restore credentials, grants, execution state or input drafts. A saved template
and a resumed UI session are different contracts.

The data-only `ryeos-ui-presentation-contract` crate lives under
`crates/clients/presentation-contract`, not the engine. It owns exact transfer
testimony and the canonical seat payload commitment. The application's existing
contract-installation hook registers its typed ThreadEvent links for daemon and
offline reset; state still owns closure traversal. Shape validation and retention
links do not authorize transfer publication. Cross-document controls stay closed
until authenticated preparation, acceptance, commit/abort, activation and replay
settlement are implemented and qualified.

Reusable templates encode selection relationships by stable mount address.
Own-set followers remain own-set followers. Cross-set followers name another
saved template and reopen only when that template has an explicit live set
resolution. Pins become named required fresh-subject inputs containing only
logical facet names: neither captured values nor fingerprints are durable.
In the current contract, every mounted tile and slot must have exactly one relationship; omission is not
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

For the shared presentation migration, move dock declarations out of reusable
view-set compositions and preserve their explicit mounted binding relationships
under the shared owner. Advance schema, compiler, saved records and fixtures
together; current per-set slot keys are not aliases for session mounts.

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

For interactive renderer work, keep the installed node running. In one terminal:

```bash
cd crates/clients/web
npm run dev
```

In a second terminal, from the same directory:

```bash
npm run dev:launch
```

Open the printed one-time URL. **Port 7411 is the live development UI; port
7400 is the installed UI and cannot show source edits.** Do not open `/ui`
directly for a fresh session. The launch helper checks that Vite is running,
mints through the existing CLI, and prints the URL without opening the desktop
browser. Keep launch tokens private. Stop Vite with Ctrl-C when finished.

Svelte and CSS edits hot-reload; other TypeScript edits can cause a full page
reload. No Cargo, sudo, install, signing or daemon restart is needed for this
presentation loop. Confirmed locally with a visible Svelte label change.

The development server binds only HTTP 127.0.0.1:7411. APIs and launch routes
proxy to the existing daemon. Its WASM wrapper and binary are also proxied from
that daemon, including Vite's `?import` requests; never substitute checkout WASM
from a different generation. Source code must match the daemon's generated
contracts and WASM exports. HMR is not a workaround for contract mismatches.

For another local node, set `RYEOS_UI_DEV_UPSTREAM=http://127.0.0.1:PORT` for both
commands and select the same node through the CLI's existing `RYEOS_APP_ROOT`.
The helper refuses a minted URL from another origin. The supported development
port remains 7411; stop the old manual-refresh asset proxy if it occupies it.

This is an explicitly local, unsigned development renderer, not release or
execution qualification. Its development-only CSP permits injected styles and
the local HMR WebSocket. Production CSP, signing, bindings, reducer/effect
ownership and admission remain unchanged. Never expose the proxy externally
or broaden worker grants for it. No mock execution state is introduced.

Before production publication, run the focused renderer checks:

```bash
cd crates/clients/web
npm run check:renderer
npm run build:renderer
```

The build writes a private generation to `target/ui-browser-stage`; it does not
publish `pkg/`. The stage validator rejects extra chunks, source maps, bare
imports, external URLs and any lazy chunk other than `ryeos_three.js`.

`scripts/dev-ui-assets.sh` serves an already assembled generation with manual
refresh; it is not the source hot-reload workflow above. Use staged/installed
generation checks separately when qualifying production assets.

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

Operational projections preserve each domain owner's visibility rules. A seat
cookie or compiled UI admission is not permission to read every principal's
records. Schedule lists and dimension summaries use the existing scheduler's
execution-authority owner filter with the authenticated caller principal, not
the node identity, source signer or selected project. They report their owner
and coverage explicitly; empty caller-owned results never mean the node has no
schedules. Scene labels must retain that scope. Do not add a competing scheduler
read owner or a new production dependency just to name an App-owned DB type.

Generic execution tables activate exact `selection.thread_id` inspection.
They do not retarget `input.route` or assume a transcript exists for every
execution kind. Conversation Views remain explicit compositions. Shared Config
revocation rejects future seat/dispatch admission; a surviving Surface binding
cannot replace that root. Already-admitted execution owners retain their normal
lifecycle, rather than being cancelled by presentation revocation.

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

Field source availability comes from the exact mounted source's Rust parameter
resolution. An unresolved scoped subject is not an in-flight request; display
the projected `unresolved` phase rather than inventing Loading or empty results.
Scoped references inside arrays and objects follow the same suppression rule.
Source phase and omission warnings are canonical projections, not deductions
from browser payload counts.

Retained artifact coordinates are evidence: exact thread ID, numeric artifact
ID and an optional recorded content hash. A missing hash is explicitly not
recorded, not a browser-derived value. Displaying a coordinate does not admit
fetching or opening it. Such actions require an ordinary signed affordance and
the existing artifact/content authority owner. Accessibility DOM identities
must include the exact mounted instance as well as the entity ID, since several
Views may simultaneously present the same retained entity.

## Saved Surface capture

Whole-Surface capture uses the visible invoking mount's retained Surface
destination, not the currently focused tab. Capture preserves ordered tabs,
their logical selection relationships and the independently retained authored
Surface origin. The origin is re-resolved on opening; it is not an attachment,
grant or permission snapshot. An unregistered project cannot silently become
projectless during saving.

Reusable Surface tabs also retain their own stable authored origins and
registered project identities. This mapping is mandatory and exact over the
saved tab IDs. Mixed-origin tabs are recompiled independently on opening;
neither the Surface origin nor the focused tab supplies missing authority.

The ordinary library View declares `save_active_surface` and
`save_particular_surface` operations and their signed persistence companions.
Reusable capture stores portable compositions, not pinned subject values.
Particular capture stores referenced particular-set records and the Surface
record atomically through the existing principal config update, with separate
expected revisions for both libraries. The complete binding request must fit
its admitted transport bounds before any effect is issued.

Reusable Surface opening must admit the origin afresh and require explicit
fresh subjects where needed. Do not implement it by calling particular resume,
borrowing the current tab's subject, or installing saved binding coordinates.
Views awaiting fresh required subjects remain visibly unresolved and cannot
execute through that dependency. Missing inputs do not prevent displaying the
whole Surface; unknown or contradictory supplied inputs refuse.
Capture support alone does not establish opening or runtime qualification.

Presentation admission preparation retains its normalized selection and placement
from the signed affordance's resolved arguments. Opening uses a closed authored,
reusable or composed Surface selection. A composed selection carries the captured
portable tabs and their exact independent origins; it is not a temporary saved
library record. Placement explicitly chooses a new Surface or an atomic tab import
into an existing Surface. Import commits that destination's exact binding,
composition revision, mount frontier, insertion index and activation choice.
The daemon derives admission purpose from placement, not a renderer hint.
Particular resume retains the selected record ID, expected
library revision and, for one particular set, its authored Surface reference.
Completion echoes particular selection from the daemon's retained receipt and
must match preparation before live publication or replay. An exact creation
request ID is mandatory on every admission transition. A structurally valid
composition from a different selected record is not a valid completion.

Ordinary opening completion echoes the entire normalized request, including
creation identity and placement. Both preparation and completion must be retained
and acknowledged before their respective invocation and mount/source publication.
Import does not create an intermediate Surface, replace the destination origin,
or reset existing drafts, shared mounts, background or contextual dialogs.
All candidate tabs and descriptors are checked before one publication; a bad
later tab cannot leave an earlier tab installed. Immutable retry receipts are
distinct from current attachment ownership: moved imported Views retain their
exact admitted coordinates, while closed and unreferenced mounts do not retain
authority merely because their opening receipt remains in history.
Pending pre-ACK receipts retain their exact reserved coordinates until the
existing owner proves authoritative completion or settlement. Browser closure
or local absence cannot release that reservation.

## Rebuild boundaries

- Svelte/CSS changes hot-reload locally. Shipping them requires browser stage
  rebuild and complete asset publication, then updating the daemon that embeds
  the assets.
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
