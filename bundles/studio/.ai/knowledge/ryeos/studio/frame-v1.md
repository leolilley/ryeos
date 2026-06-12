<!-- ryeos:signed:2026-06-12T04:03:16Z:35504d17578b4fa32ce5f7d15bd118d4aa7369bd169a1b1e189307f1249575d5:OZ007gYtrKVYWMPG9ZoQnrjT65PDCAAHXm06qPejVcFNp9HNsbIaHBPFnt6F1RK7pLMeoUQIj+hUMvUr0WLjCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/studio"
name: "frame-v1"
title: "Studio Frame Contract v1"
description: "The closed widget-primitive set and view-binding schema renderers implement; content composes against this, never against client source"
entry_type: reference
version: "1.0.0"
```

# Studio Frame Contract v1

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

Engine chrome (not content-targetable in v1): input_line, completion,
notice, dock/tile chrome.

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
  tone: { field: <path>, map: {value: tone}, default: <tone> }
  detail: [<field path>...]   # key_value
  event_kinds: { <event_type>: {primary,...} }   # timeline, per kind
  default: { ... }            # timeline fallback
affordances:
  - id: <id>
    label: <label>
    invoke:
      plane: ui | rye
      # ui: facet write -- facet/value or facet/merge; whole-value "{field}" substitutes row fields
      #     ("prefix-{field}" is literal text; put formatting in the source service)
      # rye: tokens + args (registry-resolved daemon-side)
refresh: { on_hint: <kind> | on_facet: <key> }
```

Tones: `neutral | accent | good | warn | danger` (renderers map tone to
palette; content never names colors).

Timeline projection blocks may also declare:

```yaml
role: flow | boundary | pair_open | pair_close
pair_key: <field path>        # required for pair_open / pair_close
```

Roles are mechanism vocabulary only. Event-kind names stay in signed content:
the engine knows how to fold `flow`, `boundary`, and pairs, but it never knows
which runtime event kind means cognition, tool start, or tool result.

- `flow` records merge with adjacent flow records into one prose block.
- `boundary` records render as timeline separators.
- `pair_open` records emit a pending collapsed pair under `pair_key`.
- `pair_close` records with the same `pair_key` update the open pair in place.
- Unknown or absent roles degrade to line records.
- Pair roles with no resolvable `pair_key` degrade to line records.

## Laws

- Projections are flat field paths; no logic — anything conditional
  belongs in the source service.
- Unknown widgets, kinds, keys, and absent projections DEGRADE (raw +
  provenance), never error.
- Sources are substrate services invoked through the one daemon path.
- The grammar shown is the grammar held: affordances and completion are
  capability-filtered daemon-side.
