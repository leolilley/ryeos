<!-- ryeos:signed:2026-06-13T08:19:45Z:b8c8f8e94c980b7b958ab56abd624d9ce407e740e803ff24a6c20e23c66b9c57:y7XskcFD6+ga3S8OecAw5mWapo9oXKH200BsvC9xM9/20yxQR78H9oWGNuemUfalEFIYvBv45qOIRNWM6Nh5Cg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/ryeos-ui"
name: "frame-v1"
title: "RyeOS UI Frame Contract v1"
description: "The closed widget-primitive set and view-binding schema renderers implement; content composes against this, never against client source"
entry_type: reference
version: "1.0.0"
```

# RyeOS UI Frame Contract v1

A renderer implementing this contract is a full seat. Content (`view:`
items) composes against this contract; nothing semantic lives in client
source.

## Content-bindable widgets (closed set)

| Widget | Renders | Record shape |
|---|---|---|
| `rows` | selectable list | `{primary, meta?, tone?, raw}` |
| `key_value` | field/value detail | `[(key, value)]` |
| `text` | text body | string/detail fallback |
| `timeline` | append-ordered events | `{primary, meta?, tone?, role?, pair_key?, raw}` per event |
| `scene` | ambient/spatial projection | semantic scene objects |

Engine chrome (not content-targetable in v1): notice, dock/tile chrome.
Input is no longer engine chrome: it is a content capability (the `input`
block below), and a view's `completion` source drives the suggestion rows.

## View binding schema (`view:` items)

```yaml
widget: rows | key_value | text | timeline | scene
source:
  ref: <service ref>          # substrate services; never session-gated wrappers
  params: { ... }             # values may use "@facet:<key>[.path]" (explicit references)
  collection: <field path>    # records array for rows/timeline
projections:
  primary: <field path>
  meta: <field path>
  tone: { field: <path>, missing: <tone>, map: {value: tone}, default: <tone> }
  detail: [<field path>...]   # key_value
  event_kinds: { <event_type>: {primary,...} }   # timeline, per kind
  default: { ... }            # timeline fallback
selection:                    # OPTIONAL: explicit row-activation binding (rows widget)
  activate: <affordance_id>   # row activation fires this affordance; reads {record.<field>}
input:                        # OPTIONAL, SINGULAR: one transient view-local buffer per view
  id: <id>                    # unique within the view (instance keying)
  placeholder: <string>
  target_label: <string>      # OPTIONAL author label for the prompt's target strip; else derived
  feeds: { param: <name>, debounce_ms: <int> }  # OPTIONAL: buffer -> this view's own source param
  completion: { ref: <service ref>, collection: <path> }  # OPTIONAL suggestion source
  submit: <affordance_id> | route   # OPTIONAL Enter behaviour (see "Input" below)
affordances:
  - id: <id>
    label: <label>
    invoke:
      plane: ui | rye
      # ui: facet write -- facet/value or facet/merge; whole-value placeholder substitutes a payload
      # rye: tokens + args (registry-resolved daemon-side)
      # Placeholders are NAMESPACED by producer:
      #   {record.<field>} -- from selection (row activation)
      #   {value}          -- from an input submit (the buffer text; no {input} alias)
      #   @facet:<key>      -- facet reads (unchanged)
refresh: { on_hint: <kind> | on_facet: <key> }
```

### Selection (row activation)

Row activation is intrinsic to the `rows` widget but **explicit**: a view
names which affordance row-activation fires via `selection.activate`. There
is no implicit "first affordance" activation. The named affordance is fired
by the `selection` producer and reads `{record.<field>}`.

### Input (one capability, three submit modes)

`input` is the single new optional capability: a transient, view-local
keystroke buffer — NOT a facet, NOT a widget. Exactly one per view (there
is no `inputs:` list). Any widget may carry a prompt; input is orthogonal
to display. The buffer is keyed layout-neutrally by
`{view_instance_id, view_ref, input_id}`, so the same `view:` rendered
twice has independent buffers. Printable keys edit the buffer of the
**focused view instance** that declares `input`; otherwise they fall
through to the keymap. No placement is special — initial focus (e.g. the
bottom slot) is frame policy, not an input rule.

Three submit modes, all from the one capability:

1. **`feeds` (no submit)** — the buffer is a parameter to THIS view's own
   `source`; on edit (debounced by `debounce_ms`) the source refetches.
   The filter case. **One writer per source param:** if `feeds.param`
   names a param the source already declares, that is a parse error. A
   `feeds` input works read-only (no durable write) and Enter does
   nothing.

2. **`submit: <affordance_id>`** — Enter fires that content affordance with
   the buffer text as `{value}` (the `input` producer namespace). The
   affordance does the durable thing (a `ui` facet write or `rye`
   dispatch). The command-palette / vault-setter case. Blocked read-only.

3. **`submit: route`** — a reserved value naming the engine's existing
   route-fold dispatch: classify the line (slash → tokens →
   `commands/dispatch`; plain → the `input.route` invocation template),
   carry `route_seq` stale-protection, apply read-only / empty handling,
   and ratchet the route on launch. This is the chat box
   (`view:ryeos/input`). The route metadata lives in the surface's
   top-level `input.route` block (a seat facet), not in the view.

Placeholder validation runs at **binding resolution** (`selection.activate
→ X`, `input.submit → Y`), not on the affordance alone: a placeholder a
producer can't supply fails closed before runtime. The prompt's target
strip is `target_label` if authored, else derived from the bound submit
target.

Tones: `neutral | accent | good | warn | danger` (renderers map tone to
palette; content never names colors). A tone map's optional `missing` tone is
used when the configured field is absent; `default` is used when the field is
present but unmapped.

Timeline projection blocks may also declare:

```yaml
role: flow | boundary | pair_open | pair_close
pair_key: <field path>        # required for pair_open / pair_close
```

Roles are mechanism vocabulary only. Event-kind names stay in signed content:
the engine knows how to fold `flow`, `boundary`, and pairs, but it never knows
which runtime event kind means cognition, tool start, or tool result.

- `flow` records merge with adjacent flow records into one prose block; if the
  projected primary is absent, the record is skipped rather than rendered as raw
  JSON so ephemeral replay deltas do not leak into durable UI state.
- `boundary` records render as timeline separators.
- `pair_open` records emit a pending collapsed pair under `pair_key`.
- `pair_close` records with the same `pair_key` update the open pair in place.
- Unknown or absent roles degrade to line records.
- Pair roles with no resolvable `pair_key` degrade to line records.

## Backdrop (the empty-center background is a scene)

There is no "home" mode. The center is the tiling algorithm over the tile
list: an **empty center** (zero tiles) shows the backdrop; tiles fill it
otherwise; closing the last tile returns to an empty center. No mode, no
destination — `center_is_empty` is a derived query.

The backdrop is **content, not a renderer enum.** A surface declares:

```yaml
backdrop: view:ryeos/backdrop/<name>   # a normal view, widget: scene
```

The `view:ryeos/backdrop/<name>` view is a `widget: scene` view; the
renderer draws its `RyeOsSceneModel` through the **one generic scene
renderer** (the same renderer that draws the atlas and any future scene).
Each scene object — `particle` (a dot sized by `scale`), `text` (its
`label`), structural kinds — is orthographically projected into the
center rect, glyphed by kind/scale, coloured by `tone`. New backgrounds
(shard, starfield, atlas, …) are new **scene content**, never new renderer
cases or an `ambient:` enum. The slots (incl. the bottom `view:ryeos/input`
prompt) render in every state, below the backdrop.

Animation falls out of the frame clock: `RyeOsSceneModel` carries
`generation`, the loop ticks (250ms) advancing it, and the generic
renderer steps generation-keyed motion (v1: the backdrop particles
*twinkle* — a per-object size/brightness pulse). The motion is generic
(the renderer steps by `generation`); the scene only declares particles.

## Laws

- Projections are flat field paths; no logic — anything conditional
  belongs in the source service.
- Unknown widgets, kinds, keys, and absent projections DEGRADE (raw +
  provenance), never error.
- Sources are substrate services invoked through the one daemon path.
- The grammar shown is the grammar held: affordances and completion are
  capability-filtered daemon-side.
