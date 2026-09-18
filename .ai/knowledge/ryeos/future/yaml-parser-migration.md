<!-- ryeos:signed:2026-09-18T04:59:34Z:1834c5e350be641467736be5784a29e9e882961a24200a2edeb8de6be4ed2888:Ph1oPFowAU/9hk/KrJ/iqMRVdOjh0HrYzTkCIXvaQ4f1i0bimRcczOqsT2soDDBLU1eTks9clV5owK3/X1LZBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: yaml-parser-migration
title: YAML Parser Migration
description: Deferred migration contract for replacing serde_yaml without changing RyeOS configuration, artifact, or trust semantics.
entry_type: design
version: "0.1.0"
status: deferred
```

# YAML parser migration

## Status and decision

Do not replace `serde_yaml` now merely to remove the `+deprecated` build
label.

The dependency is unmaintained and should not be mistaken for a healthy
long-term foundation. However, RyeOS has no known YAML-parser defect that this
migration would fix today, and the warning does not make the current build
incorrect. A replacement would have almost no user-visible benefit while
changing parsing and serialization behavior across configuration, registries,
runtime definitions, bundles, tools, and tests.

Keep the current dependency until there is a concrete trigger and enough time
to treat the change as a compatibility migration. When that happens, preserve
RyeOS's accepted YAML language and emitted artifacts; do not treat successful
compilation as proof of compatibility.

This note is a future-work contract, not approval of a specific replacement or
version. The inventory was recorded on 2026-09-18 at commit `545c716e2`
(`Implement merged worktree remediation`).

## What a migration would actually buy

A successful migration would:

- remove an explicitly deprecated and unmaintained dependency;
- restore an upstream owner for parser defects and security maintenance;
- remove the deprecated-package line from builds and the corresponding
  unmaintained-dependency audit finding;
- preferably remove the old parser's unsafe implementation dependency from
  this path; and
- establish explicit regression coverage for RyeOS's YAML dialect.

It would not, by itself:

- add a RyeOS capability;
- fix a currently reproduced product bug;
- materially improve normal operator workflows; or
- justify changing accepted configuration semantics or serialized bytes.

That value/risk ratio is why the work is deferred.

## Current RyeOS dependency surface

The workspace dependency is declared in `Cargo.toml` as:

```toml
serde_yaml = "0.9"
```

`Cargo.lock` currently resolves it to `0.9.34+deprecated`. Twenty-three crate
manifests depend on it: 22 inherit the workspace dependency and
`crates/tools/browser-tools/Cargo.toml` independently declares
`serde_yaml = "0.9"`. The browser-tools declaration must be brought under the
workspace dependency before or as part of any migration so the repository
cannot silently carry two parser policies.

At this baseline, 108 Rust files contain qualified `serde_yaml` references.
The directly qualified API inventory is:

| API | Occurrences |
| --- | ---: |
| `from_str` | 265 |
| `Value` | 83 |
| `to_string` | 42 |
| `from_value` | 35 |
| `from_slice` | 7 |
| `Error` | 5 |
| `to_value` | 2 |
| `Mapping` | 1 |

These figures are a scale indicator, not a complete call graph: imports can
hide later uses. RyeOS also pattern-matches the dynamic `Value` representation,
including `Value::Tagged`, and indexes mappings with YAML values. A typed-only
Serde deserializer is therefore not a drop-in replacement.

The most sensitive surfaces include:

- kind, runtime, graph, directive, parser, capability, isolation, and node
  configuration loading;
- verified item loading, composition, and dynamic `Value` merging;
- bundle and registry validation;
- CLI offline dispatch and signed-fixture construction;
- YAML header parsing and JSON-to-YAML rendering in handler tools;
- remote descriptor and other operator-facing YAML emission; and
- tests that depend on round trips, omitted fields, error messages, or emitted
  structure.

Before migration, audit every `to_string` caller for whether its bytes are
stored, signed, hashed, compared as a fixture, or shown to an operator. Semantic
round-trip equivalence is not enough for any byte-sensitive consumer.

## Replacement investigation snapshot

This section records what was true on `2026-09-18`. Re-evaluate it from primary
sources at implementation time; parser maintenance and compatibility claims are
time-sensitive.

### `noyalib-serde-yaml` / `noyalib`

The strongest drop-in candidate found in this investigation was
[`noyalib-serde-yaml`](https://github.com/sebastienrousseau/noyalib-serde-yaml),
backed by [`noyalib`](https://github.com/sebastienrousseau/noyalib). The
project's v0.0.44 documentation advertises:

- a package-rename replacement retaining the `serde_yaml` crate name;
- a `compat-serde-yaml` behavior shim;
- `Value`, `Mapping`, `Number`, and custom-tag support;
- pure Rust with no `unsafe` code; and
- MSRV 1.86, below RyeOS's pinned Rust 1.95 toolchain.

The proposed spike shape would be an exact, reviewable pin:

```toml
serde_yaml = { package = "noyalib-serde-yaml", version = "=X.Y.Z" }
```

This is a candidate, not the present decision. At the time of investigation it
was still a rapidly changing `0.0.x` ecosystem: versions 0.0.39 through 0.0.44
were published between 2026-09-07 and 2026-09-17. Its compatibility claims are
useful evidence but do not substitute for RyeOS-owned tests. Reassess release
cadence, issue history, maintainer continuity, dependency graph, licenses,
security posture, fuzzing, and exact behavior when the work is activated.

### Other approaches

- A maintained typed Serde YAML implementation is insufficient if it lacks a
  dynamic `Value` DOM, mappings with YAML keys, or tagged values.
- Moving dynamic YAML through `serde_json::Value` would narrow the data model
  and risks losing tags, non-string keys, numeric distinctions, and YAML scalar
  behavior. Do not do this as an incidental dependency cleanup.
- Forking `serde_yaml` would preserve the surface but would make RyeOS the
  parser maintainer. Consider it only for an urgent security/correctness fix
  when no maintained compatible implementation exists.
- An internal facade crate is justified only if no candidate supplies a stable
  package-rename boundary or if RyeOS needs to own policy beyond the public
  compatibility surface. Do not add a wrapper solely for architectural
  aesthetics.
- `serde_yml` is not a destination: it is itself described by its maintainers
  as unmaintained. Recheck that status rather than assuming a similarly named
  fork is maintained.

## Activation triggers

Start this migration when at least one of these is true:

1. A security advisory or correctness defect affects RyeOS's actual YAML input
   path and cannot be safely contained.
2. Dependency policy or release qualification makes the unmaintained package a
   blocking finding.
3. RyeOS needs parser behavior or platform support the current dependency
   cannot provide.
4. A replacement has demonstrated enough maintenance continuity and behavior
   compatibility to make migration risk lower than continued use.
5. The team deliberately funds a dependency-hardening pass with time for the
   complete compatibility and artifact qualification below.

The build label alone is not an activation trigger.

## Required compatibility contract

Before changing the dependency, capture a RyeOS-owned corpus from tracked,
non-secret inputs and synthetic edge cases. Run the old and candidate parsers
against the same corpus and classify every difference.

### Parsing and data-model behavior

Cover at least:

- every tracked `.yaml`/`.yml` document category and every generated YAML
  fixture used by workspace tests;
- typed deserialization through `from_str`, `from_slice`, and `from_value`;
- dynamic mappings, sequences, nulls, booleans, strings, and mapping keys that
  are not strings;
- custom tags and the exact `Value::Tagged` representation;
- integers at signed and unsigned boundaries, floating point, exponent forms,
  infinities, NaN, and negative zero where supported;
- plain-scalar resolution for `yes`/`no`, `on`/`off`, `null` forms, dates,
  timestamps, and number-like strings;
- anchors, aliases, merge keys, explicit document markers, and multi-document
  input wherever RyeOS accepts them;
- duplicate mapping keys and ordering behavior;
- unknown fields, missing fields, defaults, enum representations, flattened
  structs, and `deny_unknown_fields`;
- empty documents, BOMs, CRLF, Unicode, comments, block scalars, quoting, and
  deeply nested or large inputs; and
- malformed and hostile input, including depth/alias/resource limits and panic
  resistance.

For each difference, choose and record one disposition:

- **must preserve** — existing behavior is part of the RyeOS contract;
- **safe correction** — candidate behavior is preferable and all affected
  inputs/artifacts are intentionally migrated;
- **not observable** — proven irrelevant outside the test harness; or
- **blocker** — do not land the candidate.

### Serialization and artifact behavior

For every `to_string` and `to_value` use, establish whether the required
contract is semantic or byte-for-byte. Test:

- round-trip equality;
- key order and tag emission;
- quoting and scalar style;
- document prefix/suffix and trailing newline;
- omitted versus explicit-null fields;
- multiline strings and Unicode;
- stable error-free emission of every operator-facing document; and
- any bytes that enter signatures, digests, caches, registry identity, bundle
  publication, or golden fixtures.

If parser output currently reaches a signature or digest indirectly, preserve
the exact bytes or explicitly version and migrate that artifact. Never allow a
dependency swap to invalidate installed or published content silently.

### Errors and operational behavior

Inventory error strings and source locations exposed through CLI, daemon/API,
logs, and tests. Exact wording only needs preservation where it is an asserted
or documented interface, but errors must retain actionable path and location
context. Also compare parser throughput, peak memory, recursion behavior, and
malformed-input handling on representative large documents.

## Recommended future implementation sequence

1. Re-run the dependency and call-site inventory from a clean current baseline.
2. Re-evaluate maintained parsers and advisories from primary sources. Record
   the selected version, license, MSRV, repository health, and dependency tree.
3. Move browser-tools onto the workspace-owned dependency declaration.
4. Add the compatibility corpus and behavioral tests while still using
   `serde_yaml`; prove those tests describe current accepted behavior.
5. Build a disposable branch/worktree spike using the candidate's exact version
   pin. Prefer a package rename that preserves call sites for the first pass so
   dependency behavior is not mixed with a source-wide API refactor.
6. Run old-versus-new differential tests and review every difference using the
   dispositions above.
7. Run formatting, clippy, all workspace targets/features, the excluded or
   separately built surfaces, WASM/browser checks, repository validation,
   bundle-set validation, signed inventory checks, and real daemon/CLI smoke
   tests.
8. Rebuild representative signed and published artifacts in an isolated test
   environment and prove compatibility with artifacts produced before the
   migration.
9. Run `cargo tree` and the repository's dependency policy tooling to prove the
   old parser is gone and only the selected parser/version remains.
10. Land the compatibility tests before or with the dependency switch. Record
    the exact validation evidence and retain an easy dependency-only rollback.

Do not combine this migration with schema redesign, YAML reformatting, bundle
format changes, or unrelated dependency upgrades. A narrow commit series makes
behavioral differences reviewable and rollback credible.

## Acceptance gate

The migration is complete only when all of the following are true:

- no `serde_yaml 0.9.34+deprecated` package remains in `Cargo.lock` or the full
  feature dependency graph;
- every Rust build surface uses one workspace-owned parser policy;
- the RyeOS compatibility corpus passes and every differential result has a
  reviewed disposition;
- all existing source, integration, repository, bundle, signed-artifact,
  browser/WASM, and real-process qualification gates pass;
- pre-migration installed/signed artifacts remain readable and valid, or an
  explicit versioned migration exists;
- no observable error regression removes the source path/location operators
  need;
- performance and resource use show no material regression on representative
  inputs; and
- the final implementation record explains the operational benefit that
  justified activation.

Until that gate can be funded and satisfied, retaining the known parser is the
more honest and lower-risk RyeOS-aligned choice.

## Reproduction commands for the baseline inventory

```sh
cargo tree --offline -i serde_yaml

rg -l '^serde_yaml\s*=' --glob Cargo.toml crates

rg -l 'serde_yaml::' --glob '*.rs' crates | wc -l

rg -o 'serde_yaml::[A-Za-z_][A-Za-z0-9_]*' --glob '*.rs' crates \
  | sed 's/.*serde_yaml::/serde_yaml::/' \
  | sort | uniq -c | sort -nr
```

These are discovery commands only. Repeat them when activating the work; the
counts in this document will age as RyeOS evolves.
