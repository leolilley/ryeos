<!-- ryeos:signed:2026-09-16T03:44:59Z:b4cf04297c564c3928a141051cf649a125fb9cdb9fa65436d214a51d0a24a09a:l5hoERQQMA87lsUX/13Te+ZWGYZDuB/kKOrz1dzNMbeJzUuaJXDdYvC3SyM+Go4B+jasC+hPbKdeRptDTNXjDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "ui-design-system"
title: "RyeOS UI Visual Language and Implementation Rules"
description: "Contributor design specification for typography, composition, components, contextual input, renderer ownership and visual qualification"
entry_type: reference
version: "1.0.0"
```

# RyeOS UI visual language and implementation rules

## Purpose and status

This is contributor guidance for building RyeOS clients. It records the design
direction agreed on 2026-09-16. It specifies intended behavior and appearance;
it is not evidence that the installed clients already conform. Implementation
progress belongs in the implementation plan and its qualification record.

Read this before editing UI composition or styling, alongside
`ui-development.md` for iteration and `signing.md` for authored-item changes.
Installed UI contracts remain in the RyeOS UI bundle's knowledge.

This guide supersedes the visual prescriptions in `.tmp/frontend/style.md`
and the visual-system section of the earlier assistant UI programme. In
particular, universal monospace, thick framing, hard-offset shadows, ambient
HUD decoration and a visible character grid are no longer the default design.
The previous programme's authority and execution contracts remain applicable.

## Art direction

RyeOS should feel like a precisely engineered instrument: composed, technical,
legible and direct. Use industrial modernism with restrained neo-brutalist
character. The character comes from exposed structure, flat planes, deliberate
proportions and sharp geometry. Warm amber/orange gives the interface identity.

Information occupies the visual centre. Controls sit where they can act on
that information. A screen should read as a continuous composition, including
when it contains several independently supplied views.

This is a visual-language correction first. Do not substitute a new dashboard,
new product vocabulary or a new work-context authority for the design work.
Establish typography, surfaces, linework and proportions using the existing
information before changing information architecture.

## Concrete visual rules

### Typography

- Navigation, headings, prose, forms and ordinary row labels use a neutral,
  compact sans-serif. Start with `system-ui, -apple-system, BlinkMacSystemFont,
  "Segoe UI", sans-serif`; this requires no font acquisition or host setup.
- Commands, code, hashes, identifiers, timestamps and technical values use
  `ui-monospace, "SFMono-Regular", Consolas, "Liberation Mono", monospace`.
  A reference embedded in a row may be monospaced without making the row's
  description or controls monospaced.
- Use font weights 400 for body, 500 for labels and 600 for headings/selected
  labels. Avoid making all labels bold. Use tabular figures for aligned numbers.
- Starting scale in CSS pixels: body 14/20, secondary 13/18, technical 12/18,
  section heading 14/20 at 600, page title 22/28 at 600. These are font-size /
  line-height pairs. Implement with rem units and preserve browser zoom.
- Uppercase is for short category annotations, if needed; use modest tracking
  (approximately 0.06em). Navigation, page titles and prose use sentence case.
- Long paragraphs should stay around 65–80 characters wide. Tables, diagrams
  and evidence may use the full available work area.
- Do not shrink text to make an overcrowded screen fit. Reflow the layout or
  reveal detail on demand. Essential text must not disappear through ellipsis
  without a keyboard-accessible way to inspect the complete value.

A self-hosted font may replace the system stack after visual evaluation. That
change must include exact font assets, license, weights and asset packaging.
No runtime font CDN or machine-specific installed-font requirement. A font
choice alone does not complete the design.

### Colour and material

Use opaque, neutral charcoal planes, warm near-white text and restrained warm
accents. Preserve the palette's warmth without tinting every background brown.
The following is the initial implementation palette, subject to measured
contrast and visual review. It is a starting specification, not a claim of
already measured accessibility.

| Semantic role | Starting value | Use |
|---|---|---|
| Canvas | `#17191A` | Main working background |
| Shell | `#131516` | Navigation and persistent shell |
| Surface | `#1E2122` | Panels requiring a separate plane |
| Raised surface | `#262A2B` | Menus, dialogs and input interiors |
| Hover | `#2B3031` | Local pointer feedback |
| Selected surface | `#332B21` | Restrained warm selection fill |
| Primary text | `#EAE6DC` | Main readable information |
| Secondary text | `#B8B7AE` | Supporting information |
| Muted text | `#92968F` | Tertiary but still readable text |
| Subtle divider | `#34393A` | Nonessential section separators |
| Strong divider | `#565E60` | Major boundaries, when necessary |
| Control boundary | `#717B7D` | A boundary required to recognize a control |
| Accent | `#E3A342` | Selected markers and primary actions |
| Strong accent | `#F08A36` | Limited stronger emphasis |
| Focus | `#F2BD67` | Keyboard focus indicator |
| Success | `#9BB981` | Positive state with a text label |
| Warning | `#E8BB62` | Attention state with a text label |
| Danger | `#EF8177` | Failure/destructive state with a text label |
| Unknown | `#B0B7BA` | Unknown/unconfirmed state with explicit wording |

The palette belongs in one renderer token layer, using the existing CSS token
owner. Map semantic VM tones to it centrally. Do not sprinkle hex values into
component functions or signed views. Do not blindly add a second token naming
system next to the existing `--ryeos-*` variables; consolidate their consumers.

Selection, keyboard focus, warning and failure are different states. Selection
uses a restrained fill and a 2px marker; focus uses a visible 2px outline with
offset that does not change layout. A selected warning row retains both meanings.
No essential distinction may depend on colour alone. Accent-filled buttons use
dark text; verify contrast rather than assuming white text is readable on amber.

### Geometry, density and linework

- Use a 4px spacing base with steps 4, 8, 12, 16, 24 and 32.
- Start with 24px desktop content padding, 16px on narrow layouts; a 48px shell
  header; 36px ordinary rows and controls; 44px touch targets. Let content expand
  rows rather than clipping wrapped labels to a fixed height.
- Use 1px separators. Reserve 2px for focus/selection indicators, not routine
  containers. Do not double borders where adjacent panes share an edge.
- Corners are square for structural planes; controls may use at most a subtle
  2px radius. Avoid pill-shaped containers and rounded-card grids.
- Group related material with alignment, space and quiet dividers. Do not draw
  complete rectangles around every section, subsection and row.
- Keep working surfaces flat and opaque. No decorative gradients, glass blur,
  bevels, glow, hard-offset shadows, scan lines or permanent grid texture.
- Use a single intentional scroll owner for the main content. Logs, large
  tables and independently inspected evidence may have bounded local scrolling.
  Do not create miniature scroll panes for status/header text.
- Sparse screens use natural-height content and deliberate remaining space.
  Do not stretch empty accordions to fill the viewport or invent activity.

### Motion, imagery and icons

The default operational surfaces have no decorative scene behind their content.
Keep explicit topology/field/scene views available as real views. Their visual
content is part of their purpose and must not be erased by global CSS.

Use short 120–180ms transitions for local feedback. Streaming text and ordinary
data refresh should not move or animate whole containers. No continuously
pulsing frame, spinning ornament or activity-driven corner growth. Respect
reduced motion and show real loading stages without fabricated percentages.

Reuse existing code/vector icon mechanisms. Use consistent stroke weight and
small, legible silhouettes; label ambiguous controls. Do not invent a new logo,
icon library or bitmap decoration as part of this refinement.

## Component anatomy

| Component | Required treatment | Avoid |
|---|---|---|
| Navigation | Quiet shell plane, aligned labels, one restrained active marker; full keyboard access | Each destination in an outlined box |
| Page header | One authored human title, optional concise context, aligned actions | Slug, region name and title all repeated |
| Section | Heading/count on one baseline, natural-height rows, optional single divider | Accordion rectangles for every group |
| Row/table | Clear primary label, aligned secondary values, explicit local actions; retained identity in details | Full-row amber text, action implied by colour alone |
| Status | Short label plus small marker; unknown and stale distinguished | Large badges everywhere; green inferred from absent errors |
| Button | Compact rectangular control; a primary action uses accent sparingly; subdued secondary actions | Orange outlines on every action |
| Text field | Explicit label, coherent boundary, stable focus, inline validation and preserved draft | Placeholder as the only label |
| Composer | Declared target, editable body, delivery state and exact submit action | Permanent generic input beneath unrelated views |
| Activity/feed | Clear authorship, grouping and timestamps; technical detail expandable | Every event in a separate bordered card |
| Evidence/diff | Monospace where appropriate, aligned data, readable changes and preserved provenance | Decorating evidence until values are hard to compare |
| Dialog/drawer | One boundary, clear title/action hierarchy, deterministic focus return | Nested frames and ambiguous global submit |
| Empty/error state | Brief local explanation and an available relevant action, if any | Fake zeroes, raw stack traces as page content |
| Boot/reconnect | Same typography/palette, concise actual stage, bounded error details | A separate ornamental launch aesthetic |

Every interactive component needs idle, hover, keyboard-focus, selected where
applicable, disabled-with-reason, loading and error treatment. Disabled must
remain readable. Do not confuse disabled with unauthorized: eligibility and
its reason come from the compiled UI contract.

## Composition and contextual input

Home should present its signed observations with one page title. It has no
permanent bottom composer. Empty sections stay compact. Exact references and
provenance remain inspectable even when a friendlier authored title is primary.

The initial style specimen must retain the current information, including a
single active UI seat if that is what the source returns. Styling does not
authorize silently changing which records count as work. Any later filtering
belongs in signed source parameters or the existing authoritative projection.

Input remains an important RyeOS capability. Mount it where signed composition
declares a meaningful interaction. A work conversation can have a composer;
a review can have exact decision controls; a list can have search/filtering.
The global command palette remains available through existing bindings and
key handling without consuming a permanent bottom region.

Keep the existing route/facet and draft model. Show the addressed work/site
when relevant; retain drafts when switching targets; prevent a stale target
from silently receiving text. A missing grant must never make the renderer
substitute another command or widen authority. A malformed binding is a defect
to report and fix, not a reason to hide a failing widget with CSS.

## Ownership and constraints

| Concern | Owner |
|---|---|
| Product composition, navigation, sources, human labels, input placement and permitted actions | Signed surfaces/views in `bundles/ryeos-ui/.ai/` |
| Shared focus, selection, input routing, draft identity and semantic presentation | `crates/clients/base/src/ui/` |
| Pixels, fonts, responsive geometry, DOM, focus mechanics, CSS tokens and animation | `crates/clients/web/pkg/` |
| Cell geometry, glyphs, terminal input and terminal rendering | `crates/clients/terminal/` |
| Browser session, binding compilation, attenuation and source/action verification | `crates/daemon/ryeos-ui/` and existing API owners |
| Provider-specific content | The provider bundle |
| Contributor design rules and iteration instructions | Root `.ai/knowledge/ryeos/development/` |
| Installed user-facing UI contract | `bundles/ryeos-ui/.ai/knowledge/` |

At the 2026-09-16 audit, `RyeOsThemeVm` contains `id` and `tone`; the shared VM
sets `gruvbox-optic`. This is not a complete signed token/theme configuration
system. Existing signed presentation controls include surface border and ambient
settings and view chrome/background hints. Reuse those controls. Renderer
metrics and colours belong to the renderer's token implementation. Do not invent
a config schema, theme registry or node policy merely to change spacing/fonts.
If selectable themes become a requirement, audit the existing composition
mechanism before extending its contract. Do not describe unimplemented theme
authoring as an existing feature.

Never use `if view_ref == ...`, provider names, project names or DOM selectors
matching canonical refs to decide product behavior. Render generic semantic
widgets and signed presentation declarations. No new Observe/Operate enum,
work registry, action-profile authority or per-page executor.

Web and TUI share meaning and exact action targeting. Terminal font choice is
controlled by the user's terminal; do not require a particular font or colour
extension. Translate hierarchy into spacing, intensity and restrained rules.
Full TUI interaction remains in scope; identical physical geometry is unnecessary.

## Visual acceptance and review

Inspect real browser output at 1440×900, 1920×1080, 1024×768 and 390×844, plus
200% zoom and keyboard-only navigation. Check terminal output at 120×40 and
80×24. Data tables may scroll horizontally inside their own bounded region;
the entire page must not become wider than the viewport.

Use sparse, busy, loading, stale, error, unavailable-action and long-label
states. Include a selected item with an independent keyboard focus target.
Measure text/control contrast and inspect forced colours and reduced motion.
Normal text should meet 4.5:1 contrast; essential control/focus indicators 3:1.
Subtle nonessential separators must not be the only way to identify a control.

Review these questions against captured screens:

1. Does the page read as one deliberate composition?
2. Are type, alignment and spacing carrying hierarchy before colour/borders?
3. Is the warm accent distinctive because it is used selectively?
4. Are ordinary labels readable without resembling log output?
5. Does sparse content still look intentional?
6. Can a user distinguish selection, focus, warning, stale and unknown?
7. Is every visible input meaningfully addressed and authorized?
8. Are provenance and full technical values reachable?
9. Do Home, Work, Review, Sites, Programs, dialogs and boot feel related?

Passing DOM tests is not visual approval. Keep before/after screenshots and
review the first coherent specimen with the user before propagating the
treatment across every surface. Mock data must be labelled as fixtures.
Actual installed-session and execution acceptance must be recorded separately.
