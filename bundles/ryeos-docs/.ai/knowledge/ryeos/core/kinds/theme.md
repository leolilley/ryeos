<!-- ryeos:signed:2026-11-23T21:25:55Z:9811fca06fb81821f404fb761108b5b77c049ee8d33ff05425a9a88cf7e65a15:XMiLxs7lCVCqke/iobcxaT9zFIEIKLXH39Ph4Nif2PrjycvRauuZV0r1mtiu4XdbBThH7kRR5NCIxJtsqN5pAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/kinds
tags: [kind, theme, ui]
version: "1.0.0"
description: Theme kind reference.
---

# Kind: theme

Invariant: a `theme` is a signed, non-executable record of how an installation
looks: its colours and its pictures. The look is data; no client carries a
palette of its own.

- Directory: `themes/` (provided by the `ryeos-ui` bundle)
- Formats: `.yaml`, `.yml` via `parser:ryeos/core/yaml/yaml`
- Composer: `handler:ryeos/core/identity`
- Execution: none
- Project shadowing: refused. A project file may not take the ref of an
  installed theme, so a project can never replace a node's look or pictures.
- Ref: `theme:ryeos/night` is `.ai/themes/ryeos/night.yaml`; `id` is the file
  name.

## Body

| field | meaning |
|---|---|
| `id`, `title` | The file name, and what a person reads in a theme list. |
| `palette` | Six `#rrggbb` colours: `bg`, `ink`, `accent`, `good`, `warn`, `danger`. Every other colour a renderer draws is `ink` or `bg` at an opacity step. |
| `grounds` | Pictures for a subject that has none of its own, by slot: `<slot>: [{picture, role?}]`. A slot is an opaque lowercase word that a view's `art_slot` meets by string equality. A subject takes the entry at `sha256(subject key) mod list length` (the digest read as a big-endian number), so a record keeps its picture across sessions and across nodes with the same theme. |
| `surface_grounds` | The picture drawn behind a named Surface: `<surface ref>: {picture, role?}`, for example `surface:ryeos/ui/lens`. |

A picture is named by its `picture:` item ref and an optional rendition `role`,
never by hash. The daemon resolves the ref to the signed picture record and
supplies the hash, so a re-signed picture follows the theme without an edit.

## Choosing a theme

A principal's stored preference is `system` or the ref of an installed theme.
`config:ryeos/ui/themes` is the signed selection:

```yaml
system: { dark: "theme:ryeos/night", light: "theme:ryeos/paper" }
default: "theme:ryeos/night"
```

`system` follows the platform's light or dark preference through the pair;
`default` is the theme used where no platform preference is known. Each ref
must name an installed theme, or the node refuses to serve the catalogue.

`ui/ryeos-ui/config/update` refuses a theme that is not installed. A stored
value that is not `system` or an installed theme ref (the earlier palette ids
`night`, `paper`, `gruvbox`, or `light` and `dark`) reads as `system` with a
logged warning, and the next update writes a valid value back. Nothing
migrates the old value.

`ui/ryeos-ui/config/get` returns the installed themes as `theme_catalogue`
beside the stored `theme`: each theme's ref, title and palette, and the
`system` pair and `default`. For a browser session it also returns the Surface
grounds of the themes that session may draw now (the stored theme, or both of
the `system` pair), each resolved and granted to the session, or marked
`picture_state: unavailable` when its binding did not resolve. A failed ground
is never replaced by another picture.

## Shipped themes

`night`, `paper` and `gruvbox` ship in the `ryeos-ui` bundle. They are the
single source of the palettes: the browser renderer embeds the same files only
to draw a boot palette before the catalogue arrives.
