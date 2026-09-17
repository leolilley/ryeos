# Workspace visual study

This is an isolated, interactive HTML/CSS design prototype, not the installed
renderer and not execution evidence. All statuses, conversations, changes and
projects are synthetic. It makes no network or daemon calls.

The study preserves RyeOS's Gruvbox palette and explores typography, tile
chrome, optional navigation slots, workspace tabs, contextual input and the
launcher. Overview/Development and the launcher can be opened; other controls
are appearance specimens. No production implementation is claimed.

Open `workspace.html` in a browser. Use the top workspace tabs, Launch button,
or Ctrl/Cmd+K. The navigation-slot button toggles the left region.

System Arial/Helvetica and Liberation Mono are prototype font choices. Final
type selection and portable font assets remain to be reviewed.

## Approved direction and implementation boundary

The user approved this visual treatment on 2026-09-16, then approved supporting
arbitrarily nested splits and tabbed view groups in the shared workspace model.
Keep Gruvbox, the launcher, optional slots and the composable working environment.
The fixed CSS grid here is only a visual specimen, not the production layout
algorithm. Its hardcoded labels, data and workspace switching must not become
renderer product logic.

Production must reuse the existing surface/compiler, layout, view-instance,
input and binding owners. In particular, the prototype's composer does not
prove destination isolation, and its tab labels do not prove retained tab state.
The next qualification screenshot must use the actual renderer and shared model.

The production visual fixture uses `body.heading` for static introductory content
(`title`, optional `eyebrow`, `summary`, and `metadata` string list). This is
separate from the navigation title. It is authored content, not inferred worker
state; the fixture's task and message data remain synthetic. Dynamic task context
must come through admitted source projections, not by updating this static body.
Timeline author labels reuse ordinary line records and their metadata; prose
continues to use flow records. No provider-specific renderer is involved.

The latest web pass separates frame headers from group tabs, uses an inset
composer, and keeps session diagnostics in the bottom strip's disclosure.
The next pass adds static `body.supplement` content: frame label/detail, footer
and optional field/value footer rows, and a titled code excerpt (field/value/tone
lines). An adjacent `body.scene` uses the existing scene compiler; its compact
SVG projection draws the same points, segments, labels and glyphs, not a separate
execution graph. Row and section projections may select `glyph` and `secondary`
fields from their source records. All strings render as text, never HTML.

The preview deliberately uses the reference's sample content so visual spacing
can be compared directly. Static supplements are not live worker evidence.
Live source composition and terminal introductory/supplemental-content parity
remain unfinished; the diagram and excerpt screenshot do not qualify them.

Contributor rules: `.ai/knowledge/ryeos/development/ui-design-system.md`.
Full plan in the main checkout:
`.tmp/ryeos-ui-visual-language-and-composition-implementation-plan.md`.
