<!-- ryeos:signed:2026-11-18T23:26:19Z:a0747e83b6e0346d470f516f0caf1e08f8596e51f892a425a3f93eea55021f17:WHdUOQVkvvTje0JF6v2zO5zM+iitHFNzwHeTJ59b0Em5uxp4FWn3YavZxyZKSd2jZE0A2fy3sjK1K/gs55IKBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/ryeos-ui"
name: "frame-v2"
title: "RyeOS UI Frame Contract v2"
description: "The closed widget-primitive set and view-binding schema renderers implement; content composes against this, never against client source"
entry_type: reference
version: "2.1.0"
```

# RyeOS UI Frame Contract v2

Frame contract version `2.1.0`, implemented with UI binding contract
`ryeos.ui.binding.v14` and timeline contract `ryeos.timeline.v1` (the
`timeline-v1` document). A renderer and the daemon cut over together: the
frame, the binding contract and the timeline contract each have exactly one
version in force, and there is no reader for an earlier one.

A renderer implementing this contract is a full seat. Content (`view:`
items) composes against this contract; nothing semantic lives in client
source.

The effective signed surface supplies separate `capabilities.sources` and
`capabilities.affordances` ceilings. The daemon compiles that surface and its
effective view closure into a session-bound dispatch table. Renderers send
binding coordinates and bounded producer payloads; they never select target
refs or mint authority. A child surface may retain its parent's source ceiling
and set `affordances: []`, yielding an observation-only binding without a UI
mode flag.

## Content-bindable widgets (closed set)

| Widget | Renders | Record shape |
|---|---|---|
| `rows` | selectable list | `{primary, meta?, tone?, raw}` |
| `key_value` | field/value detail | `[(key, value)]` |
| `text` | text body | string/detail fallback |
| `timeline` | the daemon-folded timeline | items of the timeline contract: `{item: entry\|cluster\|recap, ...}` |
| `field` | multi-source entity/relation evidence field | `ryeos.ui.field.facts.v1` documents |

Engine chrome (not content-targetable in v1): notice, tile chrome.
Navigation is not chrome: a list of destinations is an ordinary view placed in
the layout like any other, so surfaces declare no separate
navigation list and renderers draw no navigation rail.
Input is no longer engine chrome: it is a content capability (the `input`
block below), and a view's `completion` source drives the suggestion rows.

## View binding schema (`view:` items)

```yaml
widget: rows | table | sections | key_value | text | timeline | field
sources:
  default:                    # channel name; multi-source views use additional named channels
    ref: <service ref>        # substrate services; never session-gated wrappers
    role: <projection role>   # OPTIONAL inert renderer projection; never execution authority
    params: { ... }           # values may use "@facet:<key>[.path]" (explicit references)
    dynamic_parameters: []    # OPTIONAL bounded keys the renderer may vary at fetch time
    activation: initial | on_demand # OPTIONAL; omitted means initial
    requires_project: true     # OPTIONAL; daemon omits this coordinate when projectless
    collection: <field path>  # records array for rows/timeline
    refresh: { on_hint: <kind> | on_facet: <key> } # optional per-channel override
projections:
  primary: <field path>
  meta: <field path>
  tone: { field: <path>, missing: <tone>, map: {value: tone}, default: <tone> }
  detail: [<field path>...]   # key_value
  default: { ... }            # fallback projection for rows
selection:                    # OPTIONAL: explicit row-activation binding (rows widget)
  activate: <affordance_id>   # row activation fires this affordance; reads {record.<field>}
input:                        # OPTIONAL, SINGULAR: one transient view-local buffer per view
  id: <id>                    # unique within the view (instance keying)
  placeholder: <string>
  target_label: <string>      # OPTIONAL author label for the prompt's target strip; else derived
  feeds: { param: <name>, debounce_ms: <int> }  # OPTIONAL: buffer -> this view's default source param
  completion: { ref: <service ref>, collection: <path> }  # OPTIONAL suggestion source
  submit: <affordance_id> | route   # OPTIONAL Enter behaviour (see "Input" below)
affordances:
  - id: <id>
    label: <label>
    producer: selection | input | tokens # OPTIONAL when unambiguous from the view binding
    visible_when: <condition>  # OPTIONAL; see "Affordance eligibility"
    enabled_when: <condition>  # OPTIONAL; see "Affordance eligibility"
    disabled_reason: <string>  # OPTIONAL; shown when visible but not enabled
    requires_project: true     # OPTIONAL; daemon omits this coordinate when projectless
    invoke:
      plane: ui | rye
      # ui: facet write -- facet/value or facet/merge; whole-value placeholder substitutes a payload
      # rye: one signed ref + args (registry-resolved daemon-side)
      # Placeholders are NAMESPACED by producer:
      #   {record.<field>} -- from selection (row activation)
      #   {value}          -- from an input submit (the buffer text; no {input} alias)
      #   @facet:<key>      -- facet reads (unchanged)
refresh: { on_hint: <kind> | on_facet: <key> } # default for channels without their own refresh
field_state:                  # OPTIONAL; field widget only; composes atomically
  cursor_scope:
    id: <scope-id>
    subject: ["@facet:<key>[.path]", ...]
```

### Shared field cursor scope

A field view may bind its cursor parameter to `@field:cursor` and join a signed
`field_state.cursor_scope`. Views with the same scope ID in one surface instance
share one causal cursor and playback state in both web and terminal. Every
member refreshes before playback advances. The nonempty `subject` list names
the facet values that define what is being replayed; any subject transition
resets the whole scope to live and advances its generation, so switching away
and back cannot resurrect a cut from another selected subject.

`field_state` composes as one root-last value. Scope IDs are bounded lowercase
identifiers, subject bindings are bounded and unique, and peer declarations for
one ID must have exactly the same subject list. A non-field declaration, a
scope with no participating `@field:cursor` source, or divergent peers fails
content validation/degrades visibly rather than creating independent cursors.

### Affordance eligibility

`visible_when` and `enabled_when` are presentation eligibility over the
affordance's record, never authority: an affordance the surface does not admit
is absent from the compiled binding whatever they say. A condition is
`{field: <path>, equals: <scalar>}` (the record's field at the path equals a
JSON scalar) or `{all: [<condition>, ...]}` of one to eight such comparisons,
nested at most one level. An affordance with no `visible_when` is always
visible; one that is visible with no `enabled_when` is enabled. A visible,
disabled affordance is drawn disabled with its `disabled_reason` (at most 256
characters). A renderer never decides eligibility itself.

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
through to the keymap. No placement is special — initial focus (e.g. a
composer tile) is frame policy, not an input rule.

Three submit modes, all from the one capability:

1. **`feeds` (no submit)** — the buffer is a parameter to THIS view's own
   `source`; on edit (debounced by `debounce_ms`) the source refetches.
   The filter case. **One writer per source param:** if `feeds.param`
   names a param the source already declares, that is a parse error. A
   `feeds` input works with an observation-only binding (no durable write) and Enter does
   nothing.

2. **`submit: <affordance_id>`** — Enter fires that content affordance with
   the buffer text as `{value}` (the `input` producer namespace). The
   affordance does the durable thing (a `ui` facet write or `rye`
   dispatch). The command-palette / vault-setter case. Blocked when the
   compiled binding contains no matching affordance coordinate.

3. **`submit: route`** — a reserved value naming the engine's existing
   route-fold dispatch: classify the line (slash → tokens →
   `commands/dispatch`; plain → the `input.route` invocation template),
   carry `route_seq` stale-protection, apply compiled-binding / empty handling,
   and ratchet the route on launch. This is the chat box
   (`view:ryeos/input`). The route metadata lives in the surface's
   top-level `input.route` block (a seat facet), not in the view.

A route input may also name `command_submit` and `thread_control` affordance
IDs. The former must declare `producer: tokens`; the latter must declare
`producer: selection`. They are roles in signed content, not special service
refs in a renderer. The daemon compiles their exact targets into the session
binding, and a surface whose affordance capability lane is empty cannot invoke
either role.

Placeholder validation runs at **binding resolution** (`selection.activate
→ X`, `input.submit → Y`), not on the affordance alone: a placeholder a
producer can't supply fails closed before runtime. The prompt's target
strip is `target_label` if authored, else derived from the bound submit
target.

Session project markers are explicit about absence. `@session:project_root`
requires a project-scoped session and refuses otherwise;
`@session:project_root_or_null` binds the canonical project root when present
and JSON `null` otherwise. Command dispatch uses the latter so its signed
descriptor—not the UI session—decides whether a token invocation is
projectless, project-bound, or binds a selected source path into parameters.
Absent session context never triggers project discovery from the daemon's
app directory. Relative project selectors require explicit caller context.

Terminal and daemon token invocations share command compilation, including
typed input binding and descriptor-required pinned admission. Command dispatch
uses the same execution admission owner as direct execution: permission to
invoke `commands/dispatch` does not grant permission to its selected target or
bound references. Nested executions preserve verified identity, grant evidence
and origin, but acquire their own recorded-service root.

Execute and streaming requests default to `parameter_encoding: typed`.
Compiled command clients send `parameter_encoding: command`; its scalar
coercion uses the selected callee's schema after project realization and before
preflight. Clients must not normalize against a live checkout when requesting
a retained generation. This input-format choice grants no execution authority.

File/stdin input is acquired by the client and supplied as structured
`arguments`; the daemon never opens a token-selected input file. Buffered
command dispatch is not a streaming transport. An accepted command requires a
caller-retained `launch_id` before submission; clients without that recovery
coordinate must use buffered execution, not mint an identity after contact.

Tones: `neutral | accent | good | warn | danger` (renderers map tone to
palette; content never names colors). A tone map's optional `missing` tone is
used when the configured field is absent; `default` is used when the field is
present but unmapped.

### Timeline widget

A `timeline` view reads the daemon timeline service
(`service:ui/ryeos-ui/timeline/thread`, `collection: items`). The response, its
entry types, ids, paging, upserts, status lane and the states a client draws
are the timeline contract, `ryeos.timeline.v1`, and are not restated here. A
source response that does not carry exactly that version is refused: the
source fails with the code `timeline_contract_mismatch` and nothing of the
response is drawn.

A view's `limit` source param bounds how many top-level items the service
returns; a source that declares `since_chain_seq` among its
`dynamic_parameters` asks only for what changed since the window it holds. A
view may bind `selection.activate` to an affordance that widens the window; it
receives `{older, next_limit}` as its record.

The renderer receives a `Timeline` view model, not the response. Besides
`title`, `source_status`, `provenance` and `affordance_hints`, it carries:

- `entries`: the visible entries, each one of `block`, `line`, `pair` or
  `separator`, with `entry_ids`, `entry_roles` (the entry type, `cluster`,
  `recap`, or `status` for lines the client adds), `entry_indents`,
  `entry_cursors`, `entry_arrived_at_ms`, `selected` and `fold_section`;
- `entry_expandable`, `entry_expanded` and `entry_details`, the detail fields
  a view's `projections.expand.fields` names for an entry's item;
- `lane`: `{label, tone}`, what the thread is doing now, from the response's
  `lane`; absent when idle. A renderer reserves the lane's height whether or
  not it is present, so the transcript does not jump;
- `dock`: the pending asks (entries of type `ask` whose state is `pending`),
  each `{id, entry, actions}`, drawn at the tail of the tile directly above the
  composer. An ask also keeps its summary row in the flow. `actions` are the
  view's affordances that declare `visible_when` and are visible for the
  ask, other than the `selection.activate` affordance; each is
  `{label, intent?, disabled_reason?}`, with `intent` absent when the
  affordance is not enabled for the ask. The record an action reads holds
  `type`, `state`, `chain_root_id`, and from the ask's slots `approval_id`,
  `request_digest` and `accept_allowed`; an ask without `approval_id` and
  `request_digest` has no actions, and a surface that admits no affordance
  shows the ask without buttons.

## Surface chrome (`chip` and `style`)

A Surface declares how its switcher chip is marked and where its chrome sits.
Every value is a closed word; an unknown value is refused when the Surface is
composed.

```yaml
chip:
  mark: grid | key | initial        # default initial
style:
  switcher: left | top | right | hidden     # default left
  status_bar: bottom | top | hidden         # default bottom
  add_tab: true | false                     # default true
```

- `chip.mark` picks one of three client drawings: the Home grid, the node's
  key (drawn from the node key fingerprint the view model carries on the
  chip), or the title's first letter. There is no picture or art field.
- `style.switcher` and `style.status_bar` place a bar on an edge or hide it.
  The operator's `alt+t` and `alt+b` toggles can hide a bar for the session;
  they never reveal one the Surface hides. The overlay key and the number keys
  work whatever the Surface declares.
- A Surface authored in a project is clamped by the daemon: `chip.mark` becomes
  `initial`, `switcher: hidden` becomes `left`, `status_bar: hidden` becomes
  `bottom`, `add_tab: false` becomes `true`. Each change is recorded as an
  attenuated entry (`surface/chip`, `surface/style.switcher`,
  `surface/style.status_bar`, `surface/style.add_tab`). A project may still
  choose `top` or `right`. Its chip carries `authored_in_project` from the
  daemon's authorship marker, which the item cannot remove.

The view model (`ryeos.ui.vm.v3`) carries the result as the Surface chip on each
switcher entry (`mark`, `authored_in_project`, `fingerprint`), `chrome.switcher.edge`,
`status_bar.edge`, and `status_bar.key_hints` (at most seven `{keys, word}` pairs
for the focused context, replacing the single hint string).

## Attention (`ryeos.ui.attention.v3`)

The `work/attention` service answers one response, which the shell reads whole;
a response of any other version is refused and its counts and rows are dropped.

- `attention[]` is a page of the needs-you queue, oldest first, in the item shape
  `ryeos.ui.attention_item.v3`: the v2 fields plus `project_root`, the project
  the item's work ran in or null. The page length is the source's `limit`.
- `counts` holds `total`, `by_kind`, `by_project` (`{project_root, count}`, null
  for projectless) and `chains`, the distinct chain ids of the whole queue
  (capped at 500, with `chains_truncated`). The daemon counts; a client only
  tests membership in the id lists.
- `running` is what the principal launched or follows (never scheduled or child
  runs): `count`, `label`, `tone`, `by_project`, `chains`, `chains_truncated`
  and a page of `items` (`id`, `title`, `item_ref`, `project_root`, `subject`,
  `state`, `since_ms` as the head thread's start). Its statuses are the ones the
  attention config maps to the `running` category.

The shell fills each Surface chip's counts from `by_project` by the Surface's
project (a projectless Surface takes the totals), each tab's flags from its
selected work's chain, and the overlay rows from the two item lists. Counts and
flags are None when attention is unavailable or a chain list was cut short.

## Backdrop

There is no "home" mode. The center is the tiling algorithm over the tile
list: an **empty center** (zero tiles) renders the empty state; tiles fill it
otherwise; closing the last tile returns to an empty center. No mode, no
destination — `center_is_empty` is a derived query.

Backdrop removed 2026-11-08; an empty centre renders the empty state. The
ambient-scene intent is kept in the project's future notes for the GPU renderer.

## Laws

- Projections are flat field paths; no logic — anything conditional
  belongs in the source service.
- Unknown widgets, kinds, keys, and absent projections DEGRADE (raw +
  provenance), never error.
- Sources are substrate services invoked through the one daemon path.
- Surface-wide and view-local sources use the same compiled binding lane.
  A browser cookie is transport identity only and cannot call a source route
  directly. Optional source `role` values select inert client projections;
  they never select a service, capability, project, or parameter authority.
- A source enters that lane only when its resolved service is both
  `ui_read_only: true` and `state_access: read_only_existing`. The daemon
  derives this property from the verified service; content and clients cannot
  assert it. Merely listing an endpoint in a surface does not make it safe.
- Source results are retained under the exact
  `{surface/view instance, channel, parameter identity}` coordinate that
  requested them. In particular, on-demand file reads do not update a global
  "current file" slot that another tile can accidentally consume.
- `activation` and `requires_project` are signed presentation behavior. They
  replace endpoint-name guesses in clients. Projectless compilation removes
  project-required routes, sources, and affordances and records the attenuation.
- The project root is an open, pinned directory authority retained by the
  session. Dispatch rechecks its path binding and uses the descriptor-derived
  path; a browser-supplied string is never project authority.
- File lists, trees, and reads stay descriptor-relative for their complete
  traversal and refuse symlink or parent-component escapes. Tree exclusions
  come from the compiled node `ingest_ignore` policy; the UI owns no hidden
  filename or directory ignore list.
- A project switch cannot mutate an existing compiled binding. It mints an
  immutable successor session. Browsers redeem its short-lived, replay-safe same-tab launch;
  native clients replace the whole session generation, including hint streams,
  live tails, and durable seat attachment. The predecessor remains bounded by
  session expiry so a lost replacement response cannot strand the operator.
- Cookie-authenticated data endpoints do not bypass the binding compiler.
  Executable UI data is reached only by signed binding coordinates through
  `ui/invocations/dispatch`; direct cookie routes are limited to transport and
  session/seat lifecycle.
- Seat lifecycle services use `ui_dispatch: verified`, preserving the
  authenticated route context just like `session/current`. Their handlers
  resolve the live browser session and enforce ownership of its seat. This
  dispatch class verifies the service definition; it does not turn a browser
  cookie into a signing principal. `ui_dispatch: session_local` is reserved
  for targets invoked through a compiled binding, such as project actions;
  generic routes refuse those targets.
- The grammar shown is the grammar held: affordances and completion are
  capability-filtered daemon-side.
