<!-- ryeos:signed:2026-10-16T09:44:47Z:d83cf537b8f3da185bb061fc8001370bda3ef519ff542cefb75a16b386a892fe:rkX/L/QSX92PKfi4wZL8C3iNGIpu+EUF756t5FdlNUzd41YcYmoqxTqwYNgI2gW5oIzGK1lDhueGE5Mg2IE1CQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "ui-design-system"
title: "RyeOS UI Visual Language and Composition Rules"
description: "Contributor specification for Gruvbox styling, nested tiled view sets, view groups, contextual input and renderer ownership"
entry_type: reference
version: "3.0.0"
```

# RyeOS UI visual language and composition rules

## Status and interpretation

This is development guidance, not evidence of shipped behavior. It records the
corrected design agreed on 2026-09-16. Read it with `ui-development.md`,
`signing.md` and `dependency-constitution.md`.

The approved visual study is in
`crates/clients/web/tests/browser/design/`. It is standalone HTML/CSS with
synthetic content, not the production renderer. Its appearance was approved;
its controls, named view sets, grouped tabs and operational claims are not
implementation evidence. Reproduce the treatment through real shared UI
contracts, not hardcoded specimen DOM.

This revision supersedes the previous neutral-charcoal replacement palette,
blanket removal of ambient layers, page/sidebar-first composition and advice
to limit this work to the existing master/stack layout. Those directions did
not represent the user's intention. Do not use the rejected earlier screenshots
as a design target.

The full execution plan is
`.tmp/ryeos-ui-visual-language-and-composition-implementation-plan.md` in the
main checkout. This tracked guide retains the decisions even when temporary
planning files are unavailable.

## Design identity

RyeOS is a composable working environment, not a website dashboard with a fixed
sidebar and a single page. Preserve the launcher, view sets, views, tiling,
optional edge slots and authored background/scene character. Improve their
proportions and relationships rather than removing them.

One-line direction: **A precise Gruvbox working environment with editorial
typography, sharp geometry, fine structural lines and restrained neo-brutalist
character.**

Professional does not mean generic or colourless. Avoid rounded-card grids,
ubiquitous pills, neutral-white substitution, uniform bold monospace, heavy
nested boxes and arbitrary decorative metrics. Do not name the product a
cockpit or studio. Use RyeOS's own terms.

Working views can be dense, but density must be deliberate. Use a clear focal
area and supporting regions. Give prose room to read; align technical material
precisely. Do not apply marketing-scale headlines to every operational view.

## Colour: preserve Gruvbox

Use the existing renderer token owner, not a parallel theme system.

| Role | Baseline |
|---|---|
| Canvas / shell | `#1d2021` |
| Main plane | `#282828` |
| Raised / secondary plane | `#3c3836` |
| Quiet structural line | `#504945` |
| Primary text | `#ebdbb2` |
| Supporting text | `#d5c4a1` |
| Muted text / stronger boundary | `#a89984` |
| Structural orange | `#d65d0e` |
| Salient yellow | `#fabd2f` |
| Positive state | `#8ec07c` |
| Failure state | `#fb4934` |

Keep the cream foreground and warm dark planes. Orange marks active structure;
yellow supplies occasional emphasis. Neither replaces ordinary readable text.
Selection, keyboard focus, warning and failure are distinct signals and need
non-colour cues. A quiet divider need not have control contrast, but a control
cannot rely on that divider alone. Measure actual foreground/background pairs:
normal text 4.5:1, essential indicators 3:1. Do not assume small orange text
or white-on-orange passes.

Ambient/background content remains an authored surface decision. Retain the
geometric identity where declared; tune contrast and opacity so content stays
legible. Do not globally erase scenes, force every view transparent, or add
background exceptions keyed to view refs. Main reading areas may be opaque
while the surrounding view set retains its background.

## Typography and geometry

- Use sans-serif for human titles, navigation, prose, forms and row labels.
  The approved study uses Arial/Helvetica; this is the initial specimen stack,
  not a promise of identical glyph metrics on every machine.
- Use Liberation Mono / platform monospace for code, commands, identifiers,
  timestamps, short chrome annotations and aligned technical values.
  The cell-grid renderer and measurement probe remain monospace.
- Start with body 14px at 1.5 line height, ordinary labels at regular weight,
  view headings 20–24px, short tile metadata 12px. Larger 32–36px headings are
  appropriate only where actual content and available space warrant them.
- Selected labels do not all become bold. Uppercase and tracking are for short
  metadata, not all text. Use tabular figures for aligned values.
- Start with 12px region gaps, 24px content padding (16px when constrained),
  34px tile headers, a compact 28px system line and 28px status strip.
  These are web measurements, not semantic contract fields or TUI row counts.
- Use 36px minimum ordinary rows, allowing wrapped content to grow; provide
  44px touch targets. Respect browser zoom and text enlargement.
- Use square structural planes and fine 1px boundaries. Focus/selection can use
  a 2px marker. Avoid repeated full rectangles inside each tile.
- Tabs sit in a shared strip with a restrained active marker; they are not
  unrelated boxed buttons. Inactive tabs remain readable.
- Each substantial view owns its content scrolling. Headers, tab strips and
  composers remain usable. Do not turn short status content into a tiny scroller.
- Reserve subdued depth for transient overlays. The approved launcher may use
  a soft shadow and restrained backdrop blur; this is not a glass-panel theme.
- Reuse the existing code/vector visual mechanisms. Do not invent a replacement
  logo or an unrelated icon set.

A portable font addition must include licensed exact assets, supported weights
and existing asset packaging. No runtime CDN or undeclared installed-font
dependency. System stacks are acceptable until an exact family is selected.

## Composition contract: intended implementation

The agreed container hierarchy has a session owning shared presentation and
Surface instances. Each Surface instance contains top-level tabs → one live
view-set instance per tab → nested groups/views. A tab hosts a composition;
it is not the composition definition. A one-view composition uses the same
model. Group-local tabs select member views. An authored Surface defines a
collection of tabs/compositions; one open instance is one presentation, not
a new authenticated session. Session manages membership and one seat/producer
per independently presented Rust core, usable with zero Surfaces open.
No additional Window entity, collection kind or browser application model.
See `ui-development.md` for current implementation gaps and seat ownership.

Use a typed signed config under the existing config kind to author shared
presentation: shared dialog View references, four optional dock mounts and
initial Surface references. "Session shell" names a responsibility, not another
kind, user identity or execution owner. Shared launcher/shortcuts Views must
remain available with no Surface open. Surface-specific dialog Views may be
contributed with scoped identities and their own admitted bindings.

The generic opening is neutral Home/background and the shared launcher. Preserve
that quiet starting point instead of forcing a dashboard, maintenance console or
application workflow into it. Purpose-specific authored presentations and Surfaces
can provide their own Views, tab arrangements and admitted operations. They use
the same composition model and design system; they do not become a second browser
application. Installation never silently replaces the user's composition.

Home need not poll domains or contain a conversation input, catalogue or initial
tile. The background and shared launcher are a complete quiet opening. Surface
discovery is a deliberate shared dialog with bounded coverage, distinct from
the selected composition's local View library. Listing a Surface is not a grant
to open it; unavailable actions and failed sources remain visibly distinct from
empty data. Do not automatically inject installed bundle Views or tabs.
Search belongs to that View's existing source input, not a second browser
catalogue. Preserve response bounds/coverage when narrowing the query.

Presentation is not an access role. Identity, node, exact subject and current
admission govern available reads and actions; do not introduce public/operator
display modes as authority. Maintenance actions belong in explicit specialised
arrangements, with the same normal admission as any other operation.

The current focus is presentation foundations and platform-aligned inspection;
ARC and other application-specific arrangements are later consumers.
Conversation continuity, automatic context following and provider chat
integration are deferred. Existing conversation/input Views keep explicit
routes; do not encode a preferred conversation-history model into dock geometry.

A Surface normally opens as a fresh surface instance in the existing session;
adding it to the existing surface instance is explicit. A single set normally opens in
a new tab; a single view is placed in the current set. These are presentation
defaults only, never execution or credential authority.

Chrome direction agreed 2026-10-03: a compact top line identifies the actual
presentation surface instance by human-facing title (number only once its scope is real),
with node connection status at the right. The bottom line carries contextual
shortcuts and an orange-highlighted RyeOS button at the far right that opens
the launcher. Do not repeat node status and launch controls on both bars. Keep
canonical refs in inspectable details. Do not label principal fallback text as
the node. Connection chrome reports transport state, not node-wide health; use
authenticated session identity context without requiring domain polling. A future node/session details dialog must distinguish principal,
entry node, surface/attachments and per-view targets; it is not permission to
invent a generic settings framework or unrestricted configuration writes.

The view-set model supports arbitrary nested horizontal/vertical splits,
with a group of view tabs at each leaf. This is an extension of the existing
shared layout machinery, not another UI framework.

- A surface signs the available views, sources, actions and initial arrangements.
- A view set owns its live layout, view groups and selection/focus. Top-level
  tabs select view sets, not hardcoded product pages. Four optional docks are
  mounted by shared presentation outside any one Surface or tab.
- A split allocates space. A group orders view-instance references, selects an
  active member, and may carry one bounded authored label for its stable role
  in the arrangement. The renderer presents that label separately from the
  group's member tabs; it must not infer a product pane from a view ref. A view
  instance owns its local content/input state.
- The layout tree is the single placement authority. Master/stack becomes an
  arrange operation, not a second mutable ordered-layout authority.
- Move preserves view identity, draft, local state and bound subject.
  Explicitly opening another instance creates independent local state.
- Optional edge docks remain anchored supporting regions, using the same
  view/group machinery where practical. Top/left/right/bottom remain available;
  none is globally required or forbidden. Reveal, collapse and temporarily
  focus them without losing their mounted attachment. Surface/tab changes
  preserve shared mounts in the document; multiple documents need reviewed
  independent mount/producer ownership before shared mutable state is exposed.
- Contextual input belongs to the exact mounted view instance it serves. A
  bottom-slot conversation or terminal is still an ordinary view; it does not
  control whichever other view was most recently focused. Its subject and
  executable binding are explicit and remain fenced across layout changes.
- The launcher remains transient and available throughout. It can open a view,
  open alongside, or instantiate an admitted arrangement. These are local
  composition operations, not grants of executable authority.
- Named view sets are initial arrangements within the existing surface contract,
  not a new kind, registry or service. Names and contents are authored data.
- Narrow rendering may present one group at a time without destroying the saved
  tree. Hidden content remains reachable. Maximisation is temporary presentation,
  not deletion of sibling views.

Retain the existing split/rectangle utilities, instance state, draft mechanisms,
seat context and reducers wherever they satisfy these contracts. Do not implement
grouped tabs as a JS-only state store while Rust believes a different view is
active. Web and terminal must agree on the active instance and action target.

### Context and authority are not geometry

The current seat-wide route and selection are not automatically sufficient for
two independent conversations. Resolve context through existing seat/binding
owners, with explicit scope for shared selections versus retained subjects.
Do not copy executable authority into layout nodes or add a second route truth
in browser state.

Switching tabs or moving a view must not silently retarget a composer. Drafts
remain keyed to exact view/input/target identity. Display the actual destination.
A missing or stale target produces an honest unavailable state; it never falls
back to another conversation, project or command.

Layout snapshots are untrusted presentation preferences. Revalidate their view
references against the current compiled surface and re-resolve executable
bindings. Never restore credentials, grants, signed authority or observed work
facts from a browser snapshot. Treat draft persistence separately from geometry;
do not silently persist sensitive message text with a saved arrangement.

## Ownership

| Concern | Owner |
|---|---|
| Shared dialogs, four optional docks and initial Surface selections | Typed signed shared presentation config, existing config resolution and UI binding compiler |
| Surface tab collections, available views, labels, context declarations, sources and actions | Signed surface/view contracts in the UI bundle and existing schema/compiler |
| Layout tree, group edits, instance identity, focus, draft/target semantics | Existing shared client model and reducers |
| Pixel geometry, fonts, DOM, pointer gestures, renderer tokens | Web client |
| Cell geometry, glyphs, keyboard rendering | Terminal client |
| Trust, binding compilation, attenuation and invocation verification | Existing daemon UI/API authorities |
| Provider-specific content | Provider bundle |
| Contributor design/implementation rules | Root development knowledge |
| Implemented user-facing UI contract | UI bundle knowledge |

The current theme VM has an ID and tone, not a full signed token configuration.
Do not introduce a theme registry or node policy to change font size or spacing.
Layout schema bounds belong to its existing compiler; authority limits belong
to existing admission owners. Product choices belong in authored definitions.

Never branch on a provider, project or canonical view ref to select renderer
behavior. Never add an Observe/Operate mode enum, page executor or broad grant
to make an arrangement render.

## Interaction and component acceptance

Support drag-to-split/tab, tab reordering, divider resizing, moving between
view sets, closing, temporary maximisation and restoring saved arrangements.
Provide keyboard and menu equivalents; drag cannot be the only path.
A normal view/tab switch is not an execution. Dirty drafts need deliberate
retention/discard behavior, not silent loss.

Launcher groups stay open when expanded/collapsed; unavailable actions include
reasons; focus returns predictably. Content headers prefer authored human
titles, with exact refs still inspectable. Errors, stale observations and
unknown delivery must remain distinguishable from success or zero results.

Keep tile contents diverse: prose, tables, files, diffs, timelines and scenes
need appropriate internal composition. The consistent layer is typography,
palette, linework, chrome and interaction—not forcing every view into one card.

## Qualification

The standalone study is the visual target. The next screenshot gate must use
actual production components and, after the model change, an actual shared-model
projection. Mark synthetic data clearly; do not imply execution evidence.

Check the populated tiled arrangement, launcher, an alternate arrangement and
the empty view set. Include all edge slots, nested splits, grouped tabs,
long labels, unavailable actions, content plus input, focus and draft retention.
Inspect at 1600×1000 (study reference), 1440×900, 1024×768, 390×844 and 200% zoom.
Check terminal at 120×40 and 80×24. Respect reduced motion and forced colours.

Use focused JS/browser checks for asset work. Scope Rust checks to changed owners
with at most two jobs. No full build/reinstall cycle for CSS iteration.
DOM tests are not visual approval. Source tests, reviewed screenshots, installed
boot/seat acceptance and real execution evidence are distinct gates.

Explicitly deferred: floating/overlapping surface instances, cross-browser dragging,
collaborative layout editing, a new theme system and native terminal emulation.
Nested composition, tab groups, contextual input, keyboard/TUI support and saved
arrangements are not deferred behind cosmetic work.
