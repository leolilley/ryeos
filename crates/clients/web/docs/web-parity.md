# Web ryeos-ui parity ledger

The browser renderer (`crates/clients/web`) and the terminal renderer
(`crates/clients/terminal`) are two transport+render skins over the SAME shared
core in `crates/clients/base` (`RyeOsCore`: state, reducer, effects, semantic
view model, scene model, keymap). Parity means: neither skin re-implements
product logic, both consume the same view model and scene model, both drive the
same reducer through the same events, and both use the shared keymap. This doc
is the standing ledger so parity is a checked fact, not a vibe.

Baseline sweep: 2026-07-03. Kept in sync when either renderer changes.

## Effects — `RyeOsEffectKind` (18 variants, both sides covered)

Every effect the core emits is executed by an adapter on each renderer. Browser
adapter: `pkg/ryeos_effects.js`. Terminal adapter:
`crates/clients/terminal/src/app/effects.rs`. Browser-native effects degrade to
a null result on the terminal by design (there is no clipboard / URL bar / hash
in a TTY); the core still receives a well-formed effect result.

| Effect kind | Web (`ryeos_effects.js`) | Terminal (`effects.rs`) |
| --- | --- | --- |
| `fetch_dimension` | fetch | daemon fetch |
| `fetch_projects` | fetch | daemon fetch |
| `fetch_topology` | fetch | daemon fetch |
| `add_project` | fetch | daemon fetch |
| `open_project` | fetch | daemon fetch |
| `fetch_threads` | fetch | daemon fetch |
| `fetch_items` | fetch | daemon fetch |
| `fetch_source` | fetch | daemon fetch |
| `fetch_commands` | fetch | daemon fetch |
| `list_files` | fetch | daemon fetch |
| `fetch_file_space` | fetch | daemon fetch |
| `read_file` | fetch | daemon fetch |
| `invoke_action` | fetch | daemon fetch |
| `cancel_thread` | fetch | daemon fetch |
| `invoke` | fetch | daemon fetch |
| `set_location_hash` | `location.hash` (browser-only) | null result (no hash in TTY) |
| `copy_to_clipboard` | Clipboard API (browser-only) | null result (no clipboard) |
| `open_url` | `window.open` (browser-only) | null result (no URL bar) |

No reducer logic is re-implemented in JavaScript: every state transition is a
`RyeOsEvent` dispatched into `RyeOsCore`; JS only translates transport and DOM.

## Widgets — `RyeOsViewVm` variants

The core projects each bound view into a `RyeOsViewVm` variant; each renderer
draws it. Terminal reference renderers live in
`crates/clients/terminal/src/render/widgets/`.

| Widget (`type`) | Web (`ryeos_components_workspace.js`) | Terminal reference |
| --- | --- | --- |
| `rows` | `rows()` | `widgets/rows.rs` |
| `table` | `tableView()` | `widgets/table.rs` |
| `sections` | `sectionsView()` | `widgets/sections.rs` |
| `timeline` | `timeline()` | `widgets/timeline.rs` |
| `map` | `sceneMap()` | scene widget |
| `atlas` | `atlasTile()` / `atlasMap()` | scene widget |
| `placeholder` | inline title/message | placeholder |

`table` and `sections` were the two closed gaps (see below). A view whose `type`
is none of the above still degrades to a soft "Unknown view" paragraph rather
than a hard failure — the intended fallback, kept.

## Scene divergence (intended)

The center scene is deliberately NOT byte-identical between renderers: the
terminal draws the shared `RyeOsSceneModel` as a cell grid; the browser draws
the same model with DOM/CSS (and Three.js where a 3D scene is bound). Both
consume the SAME `RyeOsSceneModel` — the divergence is presentation only, not
data or logic. The empty-center backdrop is likewise scene content (not an
`ambient` enum) drawn through the generic scene path on both sides.

## Keymap

The binding table is the shared `ryeos_key_command` in
`crates/clients/base/src/ryeos-ui/keymap.rs`. Both renderers translate a native
key event into a neutral `RyeOsKeyEvent` and route it through that function
against the shared `key_context()`:

- Terminal: `crates/clients/terminal/src/app/keys.rs` (crossterm adapter).
- Web: `pkg/ryeos_shell.js` (DOM `KeyboardEvent` → `RyeOsKeyEvent`) →
  `wasm.rs::ryeos_key` → `ryeos_key_command`. The command-resolution
  fallbacks (row-cursor move vs directional focus, launcher-query edit) are
  mirrored from the terminal adapter in `wasm.rs`.

Genuinely-web bindings that stay in JavaScript (not divergence — they are
platform affordances with no terminal analogue):

- Native text entry: the input-dock `<textarea>` and the launcher `<input>`
  own their own typing/submit/completion WHILE FOCUSED (native `input`/`keydown`
  listeners in `ryeos_components_workspace.js` / `ryeos_components_chrome.js`).
  The window-level shared keymap defers to them via the `isTypingTarget` guard.
- Native activation: plain Enter on a focused `<button>`/`<a>` triggers the
  native click.
- Pointer: tile focus on mousedown, atlas pan/zoom (wheel + pointer drag),
  notice/launcher click affordances.

### Fold-key asymmetry (intended, recorded)

Plain `←`/`→` fold/unfold the turn/section under the point on the terminal feed
(`keys.rs::focused_fold_section`, a terminal-local binding applied BEFORE
`ryeos_key_command`), because the feed is a cell-grid lens. The shared keymap
itself maps plain `←`/`→` to directional tile focus, which is what the web uses.
This is intended: the browser has pointer + click-to-fold section headers and no
cell-grid feed cursor, so it keeps `←`/`→` for focus. Folding on web is reached
through section-header/turn interaction, not arrow keys. If a future change wants
web arrow-folding, the unification point is the shared keymap plus a
`SetFold`-capable context field — not a second hand-rolled web binding.

## Closed gaps (this branch, `web-parity`)

1. `table` and `sections` widgets were default-cased to "Unknown view" on web,
   so the watch console (threads-list table, thread-detail/magit sections) could
   not render in the browser. Now implemented in
   `ryeos_components_workspace.js` (`tableView` / `sectionsView`), mirroring the
   terminal reference semantics: table shares column origins between header and
   rows with a leading tone-glyph gutter (first cell foreground, later cells
   muted unless selected); sections show `▾/▸ Title (count)` headers with
   indented rows (collapsed → header only, count still reflects hidden rows),
   rows reusing the rows-widget renderer. CSS in `pkg/web-shell.css`.
2. The web reimplemented the global keymap by hand in `ryeos_shell.js`
   (hardcoded `keydown` branches). Replaced with a single route through the
   shared `ryeos_key_command` via `wasm.rs::ryeos_key`. This also picked up
   bindings the hand-rolled table lacked: `Ctrl+K` launcher (the advertised,
   tmux-safe binding), `Esc` interrupting a running head thread (was always
   close-focused on web), plain-Enter submit when the foot input has text, and
   row-cursor movement across table/timeline/sections lenses (was rows-only).

## Notes for the next sweep

- No bundle YAML or signatures were touched by this branch; nothing here rides
  the Wave 3 republish.
- There is no JavaScript test harness in the web crate and `wasm.rs` compiles
  only for `wasm32`, so the shared-keymap route and the two new components are
  verified by mirroring the terminal reference, not by an automated test. If a
  JS/wasm test harness is added later, the highest-value targets are: the
  DOM-key → `RyeOsKeyEvent` translation (`ryeosKeyName`) and the
  table/sections DOM structure against `RyeOsTableRowVm` / `RyeOsSectionVm`.
