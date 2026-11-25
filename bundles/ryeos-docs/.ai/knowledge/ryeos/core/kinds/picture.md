<!-- ryeos:signed:2026-11-22T00:22:30Z:de8c2ab8aba52be02c46973c5a72bd60cd5e6c9259427cf1aabae96533cc3948:hoLrnWcKs61NeJYH4QKh4E9CsAx8iPSCMkxOGUi/jngLk59LDSIYgMAwhCofJnpOB6Go7dfjnxl5IkEV1prNCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/kinds
tags: [kind, picture, ui]
version: "1.0.0"
description: Picture kind reference.
---

# Kind: picture

Invariant: a `picture` is a signed, non-executable record that names image
bytes by content hash. Pictures are served only from installed bundles whose
signed picture manifest and records verify.

- Directory: `pictures/` (provided by the `ryeos-ui` bundle); blobs sit under
  `.ai/pictures/blobs/<sha256>.<ext>` and are covered by the bundle's signed
  picture manifest.
- Formats: `.yaml`, `.yml` via `parser:ryeos/core/yaml/yaml`
- Execution: none
- Project shadowing: refused.
- Ref: `picture:ryeos/atlas-grey` is `.ai/pictures/ryeos/atlas-grey.yaml`.

## Record

`title`, `source_url` and `licence` (`cc0` or `public-domain`) are required;
`artist`, `date`, `derived_from` and `transform` are optional. `renditions` is a
list of `{role, sha256, media_type, width, height, bytes}`: the same picture at
several sizes, each role an opaque word (`hero`, `card`). PNG, JPEG and WebP
only; animated images, SVG and GIF are refused.

## Picture bindings

Signed content attaches a picture to a subject with a picture binding,
`{picture: <picture ref>, role?: <word>}`. The ref names a `picture` item, never
a hash. The daemon resolves it:

1. the ref must be a well-formed `picture:` ref (any other kind is refused);
2. it is looked up only among the verified bundles' records, in bundle order. A
   record whose bundle failed picture verification is not there, and a project
   can never supply one;
3. `role` must name a rendition of the record; absent means the record's first
   rendition;
4. the rendition's media type must be one of the three allowed.

Every refusal leaves the subject with `picture_state: unavailable`: the card or
Surface is drawn flat. No other picture stands in, so a broken choice is
visible instead of being replaced silently.

## Serving

`GET /ui/art/<sha256>` serves a hash only to a session whose compiled binding
names it or whose grant set holds it. A service grants a hash to the calling
session after resolving a binding; the grant set is bounded by the node policy's
`max_art_grants` (`ui_browser_sessions`, schema 6), and a full set is a typed
refusal (`picture_state: unavailable`), not a silent skip. Grants end with the
session. Every other request is the same 404.
