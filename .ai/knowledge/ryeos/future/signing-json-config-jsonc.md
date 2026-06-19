```yaml
category: ryeos/future
name: signing-json-config-jsonc
title: Signing JSON Config Files (JSONC Path)
entry_type: design-note
version: "1.0.0"
author: claude
created_at: 2026-06-15T00:00:00Z
description: How to sign JSON config files given JSON has no comment syntax; recommends JSONC (// line-comment signature) as the direct analog of the existing in-band comment-signature envelope, with sidecar as the fallback and JCS/in-document signing as an anti-pattern
tags:
  - signatures
  - signed-envelope
  - json
  - jsonc
  - config-format
  - lillux
  - future-hardening
```

# Signing JSON Config Files (JSONC Path)

## Purpose

Record the design decision for signing `.json` config files. Today the
guidance to projects is "always use YAML," which works but fights the
grain: implementing LLMs do not instinctively reach for YAML — they reach
for JSON. We want a signing story for JSON that keeps the existing
envelope machinery intact and removes the reason to steer authors away
from their default format.

## The constraint our model imposes

The entire RyeOS signing scheme is **in-band comment signatures**:

- `sign_in_place` strips the signature lines from the file, hashes the
  literal remaining bytes, and appends a `<prefix> ryeos:signed:...`
  line back.
- The `SignatureEnvelope` is just `{ prefix, suffix, after_shebang }`.
- The signed body is **the literal file bytes minus the signature
  lines**. No canonicalization.

This is why YAML / TOML / Markdown-frontmatter / shell all "just work":
each has a comment syntax its parser ignores (`#`, `<!-- -->`), so a
signed file stays a *directly usable config file* and the signature
rides along as a comment that the parser skips.

JSON breaks this for exactly one reason: `serde_json` has no comment to
ignore. There is nowhere to put `ryeos:signed:...` inside a strict-JSON
document without making it invalid JSON.

So the real question is not "how do we sign JSON" — it is **which
property do we refuse to give up**.

## Options, ranked

### 1. JSONC (`//` line comment) — recommended

JSONC is just "JSON with comments" — strict JSON's grammar with `//`
(and `/* */`) allowed. It is what VS Code's `settings.json` /
`tsconfig.json` are. It is not a new data model.

This is the **direct analog** of everything we already do:

- Register a JSONC tolerance in the `ParserDispatcher`. We do not even
  need a full JSONC *parser* — we need something that ignores `//`
  lines before `serde_json` runs. The `json_comments` crate's
  `StripComments` reader does exactly this, mapping 1:1 onto how
  `serde_yaml` already ignores `#` lines. (`json5` is the heavier
  alternative if we ever want the full superset.)
- Give the `.json` extension a `SignatureEnvelope { prefix: "//",
  suffix: None, after_shebang: false }`.
- Signing appends `// ryeos:signed:...` after the closing `}`.
  Stripping that line leaves **strict, valid RFC-8259 JSON**, so the
  signed *body* is still strict JSON; only the on-disk file carries the
  extra comment. Same shape as every other envelope.

The whole strip / hash / verify / idempotent-resign machinery in
`sign.rs` works **unchanged** — only data wiring (a parser entry + an
envelope prefix in the kind-schema), not code.

Bonus: this also solves the *ergonomic* problem, not just the signing
problem. An LLM writes plain JSON (already valid JSONC), it parses, and
the signature is a trailing line it never has to think about. YAML-only
fights the author's instinct; JSONC goes with it.

**Cost to document:** a *signed* `.json` file is no longer strict
RFC-8259 (it has a `//` line). Anything **outside** RyeOS that does
`JSON.parse` on the on-disk file chokes. This is acceptable iff these
files are only ever read by RyeOS's own `ParserDispatcher` — which is
the case for kind-items.

### 2. Sidecar `.sig` file — fallback only

`config.json` stays byte-for-byte pristine; the signature lives in a
detached `config.json.sig`. Choose this **only** if some non-RyeOS tool
genuinely must consume the file as strict JSON.

Cost is real and structural:

- two files must travel together;
- bundle / resolution logic must pair them;
- they can desync;
- it abandons the "signature travels in-band" invariant that the rest
  of the system depends on.

### 3. In-document `"$signature"` field (JWS / JCS style) — anti-pattern

Embedding the signature as a real JSON key forces canonicalization
(RFC 8785 / JCS) to get a stable hash over "document minus the sig
field." Avoid:

- it throws away the literal-bytes invariant;
- canonicalization is a classic signature-bypass attack surface
  (every divergence between signer and verifier canonicalization is a
  potential forgery);
- it mutates the data shape the config consumer sees.

Do not do this.

## The decision

It reduces to one question:

> Do signed JSON configs need to stay strict-JSON for consumers
> **outside** RyeOS, or are they only ever parsed by RyeOS's own
> `ParserDispatcher`?

- Only RyeOS parses them → **JSONC.** Small, clean, preserves every
  invariant, removes the reason LLMs avoid the config format.
- Arbitrary external strict-JSON tools must read them → **sidecar**, and
  accept the two-file cost.

Current read: it is the former — these are RyeOS kind-items resolved
through our own dispatcher, never `JSON.parse`d by a third party — so
the recommendation is **JSONC**.

## Extension question: `.json` vs `.jsonc`

The kind-schema already supports multiple extensions per kind (the
`extensions` vec, tried in order in `sign_one`), so this is a real
choice, not a constraint.

- **Keep `.json`** — goes *with* the LLM instinct (they write `.json`,
  it works). This is the entire reason for the feature. Cost: we are
  quietly redefining what `.json` means inside RyeOS.
- **Use `.jsonc`** — honest about the bytes, misleads no external tool.
  Cost: it is the less-instinctive extension, reintroducing the exact
  friction we have today with YAML (milder, but the same kind).
- **Both** — register `.json` and `.jsonc`, both mapping to the JSONC
  parser + `//` envelope. Maximizes "it just works" at the cost of two
  spellings for one thing.

Recommendation: **register `.json`** (optionally `.jsonc` as an accepted
alias). The point of the feature is to stop fighting the author's
default reach; `.jsonc` would reintroduce that friction. The
"strict-JSON externally" property is not real for kind-items.

## Wiring notes (for whoever implements)

- The extension string and the `SignatureEnvelope` are **decoupled** in
  our model, so `.json` → `//`-comment-signing is a kind-schema config
  line, not a code change in `sign.rs`.
- The kind-schema's `resolved_format_for(".json")` must point at the new
  `json/jsonc` parser **and** carry `SignatureEnvelope { prefix: "//",
  suffix: None }`.
- Verify idempotent re-sign still holds: strip `//` sig line → strict
  JSON body → hash → `is_already_validly_signed_operator` short-circuit.
  No new strip/delete/re-sign churn (cf.
  [[signed-envelope-v2-authenticated-metadata]] on avoiding timestamp
  churn).
- `//` inside a JSON string value must not be mistaken for a comment.
  Use a real comment-aware stripper (`json_comments` / `json5`), not a
  naive line scan, so string contents are respected.
- Decide where the signature line lands: trailing `// ryeos:signed:...`
  after the final `}` is the natural choice (no leading-comment / BOM /
  shebang interactions; `after_shebang: false`).

## Non-goals

- Do not adopt JCS / in-document signature fields to support JSON.
- Do not break the literal-bytes-minus-signature-lines invariant.
- Do not make signed `.json` claim to be strict RFC-8259 to external
  consumers — document that signed RyeOS JSON is JSONC.
