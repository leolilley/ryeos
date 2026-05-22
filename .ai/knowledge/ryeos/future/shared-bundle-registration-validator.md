<!-- rye:signed:2026-05-22T03:56:56Z:471a3b533b1defc121e1c760443995f71ba241f0cee86d42720a0ce286caed8e:5uU1XljVMGlrhImdy1OKQ7rBT5OAuN18TKPUUPx3ZAgUlJauGgRircsdKY_gn8f24tWYAy2tLzJQSnDTykIqDg:4b987fd4e40303ac -->
```yaml
category: ryeos/future
name: shared-bundle-registration-validator
title: Shared Bundle Registration Validator Advanced Path
entry_type: implementation_guide
version: "1.0.0"
author: amp
created_at: 2026-05-22T00:00:00Z
description: Future implementation path for extracting a shared signed bundle registration validator used by bootstrap node-config loading and bundle planning installed-state loading
tags:
  - bundles
  - planner
  - node-config
  - registration-validation
  - trust-boundary
```

# Shared Bundle Registration Validator Advanced Path

## Purpose

This note captures the optional advanced path for bundle registration loading after the planner-backed init work.

The current implementation intentionally mirrors bootstrap loader semantics in `ryeos-bundle::installed` instead of doing a broad dependency redesign. That is adequate while there are only two consumers and the validation surface is small. This future path extracts one shared validator so daemon bootstrap and bundle planning cannot drift.

## Current state

Two code paths validate installed bundle registrations:

1. `crates/core/app/src/node_config/loader.rs`
   - `BootstrapLoader::load_bundle_section()` scans `<system>/.ai/node/bundles/*.yaml`.
   - It verifies signatures/trust, parses structured YAML, enforces `section == bundles`, canonicalizes paths, rejects symlinks/non-regular files, and detects collisions.
   - This is daemon bootstrap's source of truth for effective bundle roots.

2. `crates/core/bundle/src/installed.rs`
   - `load_installed_bundle_records()` scans the same registration directory for planner/preflight use.
   - It mirrors the bootstrap semantics and additionally requires a regular signed installed `.ai/manifest.yaml` whose `provides_kinds` matches on-disk schemas.
   - This replaced raw `.ai/bundles/*` scans so ambient unregistered state no longer affects preflight.

The duplicated logic is deliberate for now: it avoided pulling `ryeos-app` into `ryeos-bundle` or moving node-config types during the deterministic-init fix.

## When to implement this

Implement this advanced path only when one of these triggers appears:

- a third caller needs installed bundle registration parsing;
- registration schema or policy grows beyond `{ kind?, section, id?, path }`;
- tests or bugs show semantic drift between bootstrap loading and planner installed loading;
- planner modes beyond `InitSourceSet` become production paths for install/replace/update;
- operators need a standalone `verify-installed` command that reports exactly the same records daemon bootstrap would use.

Do not implement this just to remove small duplication. The validator sits at a trust boundary, so a broad refactor is riskier than mirroring until reuse pressure is real.

## Goals

- Have exactly one implementation for converting a signed registration file into a canonical installed bundle record.
- Preserve fail-closed behavior for malformed registrations.
- Keep daemon bootstrap behavior unchanged except for using the shared helper.
- Keep `ryeos-bundle` independent from high-level app/daemon state.
- Make installed bundle validation reusable by planner modes and future `verify-installed` tooling.
- Keep registration validation separate from full bundle preflight; registration loading should identify trusted roots, not validate every item in every bundle.

## Non-goals

- Do not redesign all node-config section loading.
- Do not make bundles self-register; registrations still come only from the system node-config `bundles` section.
- Do not add ambient `.ai/bundles/*` discovery fallback.
- Do not weaken trust requirements to tolerate unsigned or untrusted registrations.
- Do not make installed loading return an empty list on malformed state.

## Proposed module shape

Prefer extracting a small low-level module into `ryeos-bundle`, since both `ryeos-tools` and `ryeos-api` already depend on it, and `ryeos-app` can depend on `ryeos-bundle` only if that does not create a cycle.

First check the crate graph:

```text
ryeos-bundle ──▶ ryeos-engine
ryeos-app    ──▶ ryeos-engine
```

If adding `ryeos-app -> ryeos-bundle` creates no cycle, use:

```text
crates/core/bundle/src/registration.rs
```

If it creates an undesirable dependency, create a smaller neutral crate:

```text
crates/core/bundle-registration/
```

The neutral crate should depend only on:

- `anyhow`
- `serde`
- `serde_yaml`
- `ryeos-engine` for signature envelope, trust classes, and trust verification

Avoid depending on `ryeos-app`, daemon state, service handlers, or planner types.

## Shared types

Suggested shared type names:

```rust
pub struct BundleRegistrationRecord {
    pub name: String,
    pub registration_path: PathBuf,
    pub bundle_root: PathBuf,
}

pub struct BundleRegistrationOptions {
    pub require_kind_node: bool,
    pub allow_missing_id: bool,
}
```

Defaults should match current writer output and loader policy:

```rust
impl Default for BundleRegistrationOptions {
    fn default() -> Self {
        Self {
            require_kind_node: true,
            allow_missing_id: true,
        }
    }
}
```

`allow_missing_id` remains true for backward compatibility with older records whose identity is the filename stem. If `id` is present, it must always match the filename stem.

Internal parse body:

```rust
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BundleRegistrationBody {
    #[serde(default)]
    kind: Option<String>,
    section: String,
    #[serde(default)]
    id: Option<String>,
    path: PathBuf,
}
```

## Shared API

Expose two levels of API.

### 1. Validate one file

```rust
pub fn load_bundle_registration_file(
    path: &Path,
    trust_store: &TrustStore,
    options: &BundleRegistrationOptions,
) -> Result<Option<BundleRegistrationRecord>>
```

Behavior:

- Reject symlinks and non-regular files.
- Ignore files whose extension is not `.yaml` or `.yml` by returning `Ok(None)`.
- Derive `name` from filename stem.
- Read file as UTF-8 text.
- Verify signature with YAML envelope `{ prefix: "#", suffix: None, after_shebang: false }`.
- Require `TrustClass::Trusted`.
- Strip signature lines.
- Parse YAML with `deny_unknown_fields`.
- Require `section == "bundles"`.
- If `kind` exists and `require_kind_node`, require `kind == "node"`.
- If `id` exists, require `id == filename_stem`.
- Require `path` absolute.
- Require `path` exists and is a directory.
- Canonicalize `path` and store it as `bundle_root`.

The function must not inspect `.ai/manifest.yaml`; that is bundle-root validation, not registration-file validation.

### 2. Load a section directory

```rust
pub fn load_bundle_registration_dir(
    node_bundles_dir: &Path,
    trust_store: &TrustStore,
    options: &BundleRegistrationOptions,
) -> Result<Vec<BundleRegistrationRecord>>
```

Behavior:

- Missing directory returns an empty vector.
- Read immediate children only.
- Call `load_bundle_registration_file()` for each entry.
- Sort by `name` for deterministic output.
- Detect collisions by name and canonical path.
- Fail closed on any malformed YAML file in the section directory.

Collision errors should include both registration file paths and both names/paths.

## Bundle-root validation API

Keep installed manifest validation in `ryeos-bundle`, layered above the shared registration parser:

```rust
pub fn validate_installed_bundle_root(
    record: &BundleRegistrationRecord,
    trust_store: &TrustStore,
) -> Result<()>;
```

Behavior:

- Require `<bundle_root>/.ai/manifest.yaml` exists.
- Reject symlink/non-regular manifest files.
- Verify signed/trusted manifest with YAML signature envelope.
- Parse manifest via `parse_manifest(bundle_root, &record.name)`.
- Require parsed manifest name matches the registration name.
- Require `manifest.provides_kinds` equals `derive_provides_kinds(bundle_root/.ai)` after sorting.

This validation should remain separate so daemon bootstrap can choose whether to require manifest validation during early startup. For planner installed loading and `preflight_verify_bundle()` compatibility, it should be required.

## Migration steps

### Step 1: Extract shared registration code

Create `registration.rs` in the chosen low-level crate.

Move these concepts out of `crates/core/bundle/src/installed.rs`:

- `BundleRegistrationBody`
- YAML signature envelope helper
- signature/trust verification for registration files
- signature stripping helper
- symlink/non-regular file rejection for registration files
- structured YAML parsing and section/kind/id/path policy
- canonical path collision detection

Keep installed manifest validation in `installed.rs`.

Expected `installed.rs` shape after extraction:

```rust
pub fn load_installed_bundle_records_with_trust(
    system_space_dir: &Path,
    trust_store: &TrustStore,
) -> Result<Vec<InstalledBundleRecord>> {
    let bundles_dir = system_space_dir
        .join(ryeos_engine::AI_DIR)
        .join("node")
        .join("bundles");

    let records = registration::load_bundle_registration_dir(
        &bundles_dir,
        trust_store,
        &BundleRegistrationOptions::default(),
    )?;

    for record in &records {
        validate_installed_bundle_root(record, trust_store)?;
    }

    Ok(records.into_iter().map(InstalledBundleRecord::from).collect())
}
```

### Step 2: Update daemon bootstrap loader

In `crates/core/app/src/node_config/loader.rs`, replace the bespoke `load_bundle_section()` scan with the shared directory loader.

Preserve the public return type `Vec<BundleRecord>` by mapping shared records:

```rust
BundleRecord {
    name: record.name,
    path: record.bundle_root,
    source_file: record.registration_path,
}
```

Do not make `BootstrapLoader::load_bundle_section()` call `validate_installed_bundle_root()` unless you explicitly decide daemon bootstrap should fail on manifest drift at this phase. If you do add it, update startup tests and document that daemon bootstrap now validates installed bundle manifests, not just registrations.

### Step 3: Keep full node-config loader behavior aligned

`BootstrapLoader::load_full()` has a second bundles-section parsing path. Either:

1. leave it alone if `load_full()` only consumes already-bootstrap-validated records; or
2. refactor bundles-section parsing in `load_full()` to call the same helper for `section_name == "bundles"`.

Prefer option 2 if the code remains small. Do not refactor routes/verbs/aliases as part of this work.

### Step 4: Add cross-module tests

At minimum:

- shared helper rejects symlink registration file;
- shared helper rejects unsigned registration;
- shared helper rejects untrusted signer;
- shared helper rejects malformed YAML;
- shared helper rejects unknown fields;
- shared helper rejects wrong `section`;
- shared helper rejects `kind` other than `node`;
- shared helper rejects `id` that differs from filename stem;
- shared helper rejects relative `path`;
- shared helper canonicalizes registered path;
- shared helper detects duplicate names;
- shared helper detects duplicate canonical paths;
- `ryeos-app::BootstrapLoader::load_bundle_section()` still loads the same valid registration fixture;
- `ryeos-bundle::installed` still rejects missing/unsigned/symlinked/drifted installed manifests.

Use the same fixture style as existing tests: generate an Ed25519 key, pin it in a temp trust store, sign registration bodies with `lillux::signature::sign_content()`.

## Backward compatibility

Existing registration records written by `write_node_bundle_registration()` have this form:

```yaml
kind: node
section: bundles
id: core
path: /absolute/canonical/path
```

The shared validator should accept these. It may also accept records without `kind` or `id` only if existing bootstrap behavior already did; if unsure, keep `kind` optional but validate it when present, and keep `id` optional but validate it when present.

Do not accept:

- unsigned records;
- records signed by untrusted keys;
- records with unknown YAML fields;
- symlink registration files;
- non-regular registration files;
- relative paths;
- paths that do not exist or are not directories;
- duplicate names or duplicate canonical paths.

## Error handling requirements

All malformed installed-state errors must fail closed.

Do not do this:

```rust
let installed = load_installed_bundle_records(...).unwrap_or_default();
```

Do this:

```rust
let installed = load_installed_bundle_records(...)
    .context("load installed bundle registrations")?;
```

Error messages should include enough operator context:

- registration path;
- bundle name;
- declared path;
- expected section/id/kind;
- signer/trust failure where applicable;
- both sides of a collision.

## Verification plan

Run focused tests first:

```bash
cargo test -p ryeos-bundle installed registration plan
cargo test -p ryeos-app node_config
cargo test -p ryeos-tools -- init
cargo test -p ryeos-api bundle
```

Then run a temp reinstall smoke test:

```bash
tmp=$(mktemp -d)
target/release/ryeos init \
  --system-space-dir "$tmp/system" \
  --user-root "$tmp/user" \
  --source bundles \
  --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml

target/release/ryeos init \
  --system-space-dir "$tmp/system" \
  --user-root "$tmp/user" \
  --source bundles \
  --trust-file .dev-keys/PUBLISHER_DEV_TRUST.toml
```

Both init runs should install/replace `core` and `standard` in planner order and should not be influenced by unregistered directories under `<system>/.ai/bundles/`.

## Rollout strategy

1. Extract the shared helper behind tests without changing behavior.
2. Switch `ryeos-bundle::installed` to use it.
3. Switch `BootstrapLoader::load_bundle_section()` to use it.
4. Run the temp reinstall smoke test.
5. Only then consider refactoring `load_full()` bundles-section parsing.

Keep each step reviewable. If the crate dependency graph becomes awkward, stop and use the neutral small crate rather than forcing a broad app/bundle dependency redesign.
