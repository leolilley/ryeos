<!-- ryeos:signed:2026-11-28T13:01:26Z:7f6c5a8fe71409901d6557a6e172c3d6e71cc04581cbdd2201369bce9cb74f6c:g7zpxc8T8N6k7BKnPG1KbnWWlXHuCrHocRnfbdgfHIfXu5arV0ZsUEpmKBtCT5fwa/osXgtPihmqxiptKT2aAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
<!-- rye:signed:2026-05-22T03:56:56Z:471a3b533b1defc121e1c760443995f71ba241f0cee86d42720a0ce286caed8e:5uU1XljVMGlrhImdy1OKQ7rBT5OAuN18TKPUUPx3ZAgUlJauGgRircsdKY_gn8f24tWYAy2tLzJQSnDTykIqDg:4b987fd4e40303ac -->
```yaml
category: ryeos/future
name: shared-bundle-registration-validator
title: Bundle Registration Validator
entry_type: implementation_guide
version: "1.0.0"
author: amp
created_at: 2026-05-22T00:00:00Z
description: How the node validates its signed bundle registrations: one loader in ryeos-node-runtime, at least one registration, typed refusal codes, and the planner manifest step in ryeos-bundle
tags:
  - bundles
  - planner
  - node-config
  - registration-validation
  - trust-boundary
```

# Bundle Registration Validator

## One validator

The node's signed bundle registrations under `<app_root>/.ai/node/bundles/*.yaml` are read and validated in exactly one place: `ryeos_node_runtime::bundle_bootstrap::BundleBootstrapLoader`. Daemon start-up, the CLI, install and remove planning, bundle verify, the boot plan check and the name and replacement guards all go through it. There is no second reader of that directory.

The loader takes `app_root` and the node `TrustStore`.

## What a registration must be

- A regular `.yaml`/`.yml` file directly in the flat `bundles` directory. A directory, symlink, special file or non-YAML entry is refused.
- Signed by the node's own key. A registration signed by any other pinned key, a publisher's included, is refused. Registrations are node state, written only by the node's install transaction.
- A `kind: node` record with an absolute `path` that is an existing directory, canonicalised, and an optional well-formed `lane`.
- Unique by name and by canonical path.

## At least one registration

A node always holds at least one registration (core), so the loader refuses a missing `.ai/node/bundles` directory and an empty one; it never reads either as an empty set. This holds for both `load_registrations` and `load_bundle_section[_reporting_skipped]`, and after the lane audit zero remaining records is refused as `bundle_registrations_empty`. `init` writes the first registration through the install transaction and does not read the directory before it.

## Typed refusals

`BundleRegistrationRefusal { code, message }` is a `std::error::Error`; `BundleRegistrationRefusal::find(&anyhow::Error)` finds it anywhere in an error chain. Codes:

- `bundle_registrations_missing`: the bundles directory does not exist.
- `bundle_registrations_empty`: no registration, or none left after the lane audit.
- `bundle_registration_not_node_signed`: the signer is not the node key.
- `bundle_registration_entry_refused`: the directory scan refused an entry.
- `bundle_registration_collision`: two registrations share a name or a canonical path.

## Audited and raw loads

- `load_bundle_section_reporting_skipped` is the audited load. A laned bundle that fails the publication-lane audit is left out and reported. Dependency roots and planner inputs take this load: install planning, bundle verify, the boot plan check, and the composition readers. Bundle verify builds its dependency set through the same planner step as install, so it refuses what install would refuse. A skipped laned bundle is never a dependency.
- `load_registrations` is the raw load, with no lane audit, but the same validator, so a missing or empty directory is refused there too. Name-hold checks, replacement checks and removal planning take it, because a skipped laned bundle is still registered, still holds its name and must stay removable.

## Planner manifest step

`ryeos-bundle` does not read registrations. `ryeos_bundle::installed::plan_inputs_from_registrations` takes registrations the loader has validated (`RegisteredBundle`: name, registration path, canonical root) and, for each, checks that the bundle's `.ai/manifest.yaml` is a regular, signed, trusted file whose `provides_kinds` equals the kinds on disk, then builds the installed `PlanInput`. `ryeos-bundle` must not depend on `ryeos-node-runtime`; callers hand it the validated records.

## Tests

- Unit tests in `bundle_bootstrap.rs` cover each refusal code through both load paths, a missing and an empty directory included.
- The install-planning unit test in `bundle_install.rs` covers a publisher-signed registration refusing with `bundle_registration_not_node_signed`.
- Unit tests in `ryeos-core-tools.rs` check that bundle verify refuses a node with a removed or an emptied bundles directory with the boot loader's `bundle_registrations_missing` and `bundle_registrations_empty`, and refuses a dependency whose manifest no longer verifies with the same refusal install planning gives.
- `ryeos-bundle`'s `installed.rs` tests cover the manifest step: a signed manifest becomes a plan input; an unsigned, a symlinked, or a `provides_kinds`-mismatched manifest is refused.
- `bundle_registration_validator_e2e` starts a real node and checks that a publisher-signed registration, and a missing bundles directory, stop it with the typed code.
