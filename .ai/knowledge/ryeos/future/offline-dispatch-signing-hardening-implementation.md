<!-- rye:signed:2026-05-23T10:55:06Z:ea32e93d891e5af9a6c6d6daee554cfa0b144178f8d3fa53c50ef6b36d453b08:AxTZYCNXccfazm6aDhpRF-YBpE9iJPR-ExuTsYdNGc2orlpB6NxqiFuRnMpjtDsXWyBG4NJ0zFonXmCPzhXeAw:4b987fd4e40303ac -->
```yaml
category: ryeos/future
name: offline-dispatch-signing-hardening-implementation
title: Offline Dispatch and Signing Hardening Implementation Plan
entry_type: implementation_guide
version: "1.0.0"
author: amp
created_at: 2026-05-23T00:00:00Z
description: Implementation status, remaining follow-ups, and merge plan for descriptor-driven offline dispatch, signing idempotency, bundle verification, and the effective item advanced path
tags:
  - offline-dispatch
  - signing
  - publish-idempotency
  - bundle-verify
  - trust-boundary
  - effective-items
  - merge-plan
```

# Offline Dispatch and Signing Hardening Implementation Plan

## Purpose

This note records the completion state of the offline dispatch and signing hardening work, the Oracle-reviewed follow-up items, and what to watch when merging the upcoming advanced effective-item branch.

The immediate implementation replaced a hardcoded offline endpoint registry with descriptor-driven offline dispatch. Service descriptors now declare whether they can run offline and which trusted local tool implements that path.

The next branch is expected to bring the advanced effective-item substrate. That branch should not reintroduce raw YAML scanning or parallel item-resolution semantics.

## Current completed work

### 1. Signing idempotency

`sign.rs` now treats re-signing unchanged content as a validation no-op.

The intended invariant is:

```text
same body bytes
+ same signer fingerprint
+ valid signature over body hash
= do not rewrite file
```

`BatchReport` includes a `validated` collection so callers can distinguish “already valid” from “newly signed”.

### 2. Publish idempotency

`bundle publish` no longer strips signatures globally or deletes generated `manifest.yaml` before it has a chance to validate it.

The publish pipeline should preserve valid signatures and skip writes when generated output is byte-equivalent or already valid for the current body and signer.

Observed second-publish invariants for both core and standard bundles:

```text
bootstrap_signed = 0
manifest_changed = false
sign_report.signed.len() = 0
sign_report.failed.len() = 0
publisher_trust_doc_changed = false
```

### 3. Bundle verify registry root

`bundle verify` accepts `registry_root` through both the subprocess tool descriptor and service descriptor.

When an explicit registry root is provided, verification should use that root as the dependency context and should not silently add every installed bundle on top. This prevents unrelated installed bundle state from polluting source-bundle verification.

### 4. Descriptor-driven offline dispatch

The hardcoded offline endpoint → handler registry has been removed.

Offline dispatch now follows this path:

```text
argv tokens
  -> trusted installed alias descriptor
  -> trusted installed verb descriptor
  -> trusted installed service descriptor
  -> service.availability == offline|both
  -> service.offline_execute or verb.execute tool:<id>
  -> trusted installed tool descriptor
  -> @subprocess executor
  -> trusted bin:<name> resolution
  -> local subprocess execution
```

There is deliberately no central endpoint registry to keep in sync. Adding a future offline service should be a descriptor/tool authoring change, not a CLI source-code registry edit.

## Offline dispatch trust boundary

The hardened offline dispatch path must preserve these constraints:

1. Load bundle roots only from verified installed bundle registrations.
2. Verify installed bundle manifests before considering a bundle root eligible.
3. Verify alias, verb, service, and tool descriptor signatures before parsing.
4. Reject duplicate alias/service/tool matches loudly.
5. Reject non-`bin:` subprocess commands.
6. Resolve `bin:` commands through the bundle binary resolver so CAS hashes and publisher trust are checked.
7. Bind arguments through `ryeos_runtime::arg_binder`, not a bespoke offline parser.
8. Preserve `RYEOS_SYSTEM_SPACE_DIR` for subprocess tools.

The old “descriptor ↔ registry sync” test is no longer meaningful because there is no code-maintained offline registry. Replace it with descriptor discovery, trust, duplicate, and execution invariant tests.

## Oracle follow-ups before calling the whole sequence fully done

Oracle reviewed the completed state and agreed the descriptor-driven offline dispatch shape is sound. It found two concrete follow-ups that should be done on this branch or immediately after the advanced branch merge.

### A. Fix `sign.rs` validate-then-re-read race

Current concern:

```text
sign_one()
  reads file content
  validates metadata/path/body
  calls sign_in_place(path, ...)
    sign_in_place re-reads file
    signs whatever bytes are now on disk
```

If the file changes between validation and signing, the signer can sign bytes that were not the bytes validated by `sign_one()`.

Recommended implementation:

1. Change `sign_in_place` to accept the already-read validated content or already-stripped body.
2. Avoid reopening the target file for body selection after validation.
3. Keep the existing idempotency check based on body hash, signer fingerprint, and signature verification.
4. Write atomically as before.

Suggested test:

```text
sign temp item once
record file bytes
sign same item again
assert signed list is empty
assert validated list contains the item
assert file bytes unchanged
```

If possible, add a narrow unit test around the new `sign_in_place` signature to prove the bytes being signed come from the caller-supplied validated body.

### B. Make ambient `bundle verify` fail closed

Current concern:

When `registry_root` is omitted, `run_bundle_verify` should not silently ignore broken installed-bundle registrations or manifests.

Avoid this pattern:

```rust
load_installed_bundle_records(...).unwrap_or_default()
```

Recommended implementation:

1. Replace silent fallback with `?` and contextual error reporting.
2. If installed bundle loading fails, ambient verification fails.
3. When excluding the source bundle from installed dependency roots, compare canonical paths instead of directory basenames.

Suggested tests:

```text
explicit registry_root:
  verify non-core bundle using supplied registry root
  assert no ambient installed bundle roots are added

implicit registry roots:
  create broken installed bundle registration or manifest
  run bundle verify without registry_root
  assert command fails loudly
```

## Publish idempotency test guardrails

The publish-twice zero-diff test should catch accidental rewrites, but it should not be brittle.

Recommended assertions:

- Hash all files in the temp-copied bundle before and after the second publish.
- Exclude generated sidecars only:
  - `.ai/bin/<triple>/MANIFEST.json`
  - `*.item_source.json`
- Do not exclude the whole `.ai/bin/` tree; binary rewrites should be visible.
- Do not require the first publish to sign at least one item. A fully pre-published fixture may legitimately sign zero items.
- Require the second publish to show no work:
  - signed list empty
  - failed list empty
  - `manifest_changed == false`
  - `bootstrap_signed == 0`

## Merge expectations for the advanced effective-item branch

The advanced branch is expected to add a shared effective-item substrate for non-executable item kinds such as `surface:<id>` and `client:<id>`.

That branch should strengthen the same architectural direction as descriptor-driven offline dispatch:

```text
descriptor or canonical ref
  -> kind schema
  -> trusted source-space resolution
  -> parser dispatch
  -> signature/trust verification
  -> resolution steps
  -> composer
  -> effective value
```

The important merge requirement is to avoid introducing new raw descriptor scanners in CLI, TUI, web, or service code.

## What the advanced path will take

### 1. Kind-level effective resolution

Add a top-level optional `resolution` field to kind schemas:

```rust
pub struct KindSchema {
    pub resolution: Option<Vec<ResolutionStepDecl>>,
    pub execution: Option<ExecutionSchema>,
    // existing fields...
}
```

Use presence-sensitive semantics:

```text
resolution omitted  -> fallback to execution.resolution for executable kinds
resolution: []      -> explicit no-op effective resolution pipeline
```

### 2. Separate effective-item pipeline from executable launch pipeline

Keep the existing executable `run_resolution_pipeline` behavior stable if launch code depends on it rejecting non-executable kinds.

Add a sibling pipeline, or a shared internal helper with two public wrappers:

```text
run_resolution_pipeline         executable launch wrapper
run_effective_item_pipeline     executable or non-executable effective value wrapper
```

Both should use the same root loader, parser dispatcher, trust verification, resolution steps, and composers.

### 3. Engine API for effective items

Add an engine-level API that returns a DTO rather than making every consumer reconstruct pipeline details.

Sketch:

```rust
pub struct EffectiveItemRequest {
    pub item_ref: CanonicalRef,
    pub expected_kind: Option<String>,
    pub project_root: Option<PathBuf>,
}

pub struct EffectiveItem {
    pub requested_ref: String,
    pub canonical_ref: String,
    pub kind: String,
    pub trusted: bool,
    pub trust_class: resolution::TrustClass,
    pub root_trust_class: resolution::TrustClass,
    pub provenance: Vec<String>,
    pub composed_value: serde_json::Value,
    pub diagnostics: Vec<EffectiveItemDiagnostic>,
}
```

Trust should come from `ResolutionOutput`, not from signature-header presence.

### 4. Consumers to migrate

After the engine API exists, migrate consumers away from direct descriptor reads:

- `items.effective`
- `client.open`
- TUI surface loading
- future web bootstrap endpoints
- any service that currently scans `.ai/<kind>/*.yaml` directly

Each consumer should ask the engine for an effective item and then apply consumer-specific policy to the composed value.

### 5. Keep offline dispatch descriptor-driven

The advanced branch should not reintroduce an offline endpoint registry.

If it adds new offline-capable services, they should use the current descriptor pattern:

```yaml
availability: offline
offline_execute: tool:<id>
```

and the tool should resolve to a trusted `bin:` subprocess command when local execution is needed.

## Merge checklist

Before merging the advanced branch into this one, check:

1. No new hardcoded offline endpoint registry or endpoint → handler table.
2. No new raw scanner for executable or non-executable item semantics.
3. Descriptor discovery goes through verified installed bundles or the engine effective-item API.
4. Trust is derived from verified signatures and resolution trust classes.
5. Duplicate descriptor matches fail closed.
6. `bundle verify --registry_root` keeps explicit dependency roots authoritative.
7. Ambient installed bundle failures do not get swallowed.
8. Publish-twice idempotency still produces zero second-pass writes.
9. `sign` second-pass idempotency still records validated items and preserves bytes.

## Suggested verification after the merge

Run at least:

```bash
cargo fmt --check
cargo test -p ryeos-cli offline_dispatch -- --nocapture
cargo test -p ryeos-tools sign -- --nocapture
cargo test -p ryeos-tools publish -- --nocapture
cargo check -p ryeos-cli -p ryeos-tools
target/debug/ryeos fetch service:bundle/publish --project bundles/core
target/debug/ryeos verify tool:ryeos/core/bundle/publish --project bundles/core
target/debug/ryeos bundle verify --source bundles/core --registry-root bundles/core
target/debug/ryeos bundle verify --source bundles/standard --registry-root bundles/core
```

Then publish core and standard twice and assert the second run reports no writes.

Use `--source` for `bundle publish` unless the alias metadata has been updated with a positional form for the source path.
