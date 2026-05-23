<!-- rye:signed:2026-05-23T09:40:13Z:752418732bd48b780ad9db2a7dcb447d16583e5e62e2c3f2616aa2fd0ea356ae:A1oKxP-tQSGKR05Z7ph1dtymZ8XgLEzQaPPdfBcz9z9I4aZqdMqssT2vvuMoNS0kMYM5FDh3mX1bynorFpTOCQ:4b987fd4e40303ac -->
```yaml
category: ryeos/future
name: effective-non-executable-items
title: Effective Non-Executable Items Substrate
entry_type: implementation_guide
version: "1.0.0"
author: amp
created_at: 2026-05-23T00:00:00Z
description: Future implementation path for making surface/client/config-style non-executable items use the same verified effective item substrate as executable items
tags:
  - effective-items
  - surface
  - client-open
  - item-resolution
  - trust-boundary
  - tui
  - web-ui
```

# Effective Non-Executable Items Substrate

## Purpose

This note captures the agreed advanced path for RyeOS TUI/surface work: make non-executable items first-class effective items instead of letting API, CLI, TUI, or web code invent parallel resolution behavior.

The immediate trigger was the TUI/surface architecture work. `surface:<id>` and `client:<id>` need to be real signed Rye items, but they should not be executable runtimes. They need resolution, verification, parser dispatch, composition, provenance, and policy diagnostics; they do not need thread records or execution plans.

## Core principle

Execution is one consumer of an effective item. It is not what makes an item real.

The common item path should be:

```text
canonical ref
  -> kind schema
  -> source-space resolution
  -> parser dispatch
  -> signature/trust verification
  -> item resolution steps such as extends/references
  -> composer
  -> effective composed value
```

Executable kinds then continue into planning/dispatch. Non-executable kinds stop at the effective value and are consumed by services or clients.

## Why this is needed

Without a shared effective item substrate, consumers drift:

- `items.effective` may manually call `resolve_item_full`, read YAML, strip signature lines, and treat signature header presence as trust.
- `client.open` may scan `.ai/clients/*.yaml` directly and parse raw descriptors.
- the TUI may parse local YAML and call it a `surface:` item.
- future `/ui/api/bootstrap` may add a fourth surface resolver.

That creates parallel Rye item systems. The engine must own item semantics.

## Existing machinery to reuse

The engine already has most of the correct substrate:

- `CanonicalRef`
- `KindRegistry`
- `Engine::resolution_roots`
- `Engine::effective_parser_dispatcher`
- `resolution::run_resolution_pipeline`
- `resolution::context::load_item_at`
- `ResolutionOutput`
- `ComposerRegistry`
- `KindComposedView`
- `TrustClass`
- `binary_resolver::resolve_bundle_binary_ref`

The key limitation is that the current resolution pipeline is tied to `kind_schema.execution.resolution`, so non-executable kinds cannot use it cleanly.

## Recommended implementation

### 1. Add kind-level effective resolution

Add a top-level, presence-sensitive `resolution` field to `KindSchema`:

```rust
pub struct KindSchema {
    pub resolution: Option<Vec<ResolutionStepDecl>>,
    pub execution: Option<ExecutionSchema>,
    // existing fields...
}
```

Use `Option<Vec<_>>`, not a plain `Vec<_>`.

Reason:

```text
resolution omitted  -> fallback to execution.resolution for existing executable kinds
resolution: []      -> explicit declaration that this kind has no item-resolution steps
```

Add a helper:

```rust
impl KindSchema {
    pub fn effective_resolution(&self) -> &[ResolutionStepDecl] {
        if let Some(resolution) = &self.resolution {
            resolution.as_slice()
        } else if let Some(execution) = &self.execution {
            execution.resolution.as_slice()
        } else {
            &[]
        }
    }
}
```

This keeps existing executable behavior compatible while giving non-executable kinds their own item pipeline.

### 2. Keep executable pipeline behavior stable

Do not silently change the existing launch contract.

Keep:

```rust
run_resolution_pipeline(...)
```

as the executable-only wrapper. It should continue to reject kinds without `execution:` because existing launch/executor call sites may rely on that failure mode.

Add a sibling:

```rust
run_effective_item_pipeline(...)
```

or a shared internal helper with two public wrappers:

```text
run_resolution_pipeline         executable-only launch wrapper
run_effective_item_pipeline     executable or non-executable effective value wrapper
```

Both should reuse the same root loader, resolution steps, trust verification, and composer registry.

### 3. Effective item pipeline behavior

`run_effective_item_pipeline` should:

1. look up the kind schema;
2. build an alias resolver only if the kind has execution aliases, otherwise use empty aliases;
3. load the root exactly once through `context::load_item_at`;
4. run `kind_schema.effective_resolution()`;
5. compose through `ComposerRegistry`;
6. return `ResolutionOutput`.

Do not call `Engine::resolve()` and then separately run the pipeline. That double-resolves and can observe inconsistent snapshots under concurrent edits.

### 4. Engine effective item API

Add an engine API like:

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
    pub derived: HashMap<String, serde_json::Value>,
    pub policy_facts: HashMap<String, serde_json::Value>,
    pub diagnostics: Vec<EffectiveItemDiagnostic>,
}
```

Implementation outline:

```rust
pub fn effective_item(&self, req: EffectiveItemRequest) -> Result<EffectiveItem, EngineError> {
    if let Some(expected) = &req.expected_kind {
        ensure!(expected == &req.item_ref.kind);
    }

    let roots = self.resolution_roots(req.project_root.clone());
    let parsers = self.effective_parser_dispatcher(req.project_root.as_deref())?;
    let output = resolution::run_effective_item_pipeline(
        &req.item_ref,
        &self.kinds,
        &parsers,
        &roots,
        &self.trust_store,
        &self.composers,
    )?;

    // DTO from ResolutionOutput
}
```

Trust must come from the resolution pipeline:

- `trust_class`: folded chain trust = `ResolutionOutput.executor_trust_class`
- `root_trust_class`: `ResolutionOutput.root.trust_class`
- `trusted`: true only for `TrustedSystem` or `TrustedUser`

Do not use “signature header present” as a trust signal.

### 5. Provenance/source diagnostics

The first implementation can expose minimal provenance from `ResolutionOutput.root` and `ResolutionOutput.ancestors`:

```text
provenance = ancestors resolved refs + root resolved ref
```

If callers need source space, resolved label, shadowed candidates, or bundle identity, carry those through the pipeline root load. `context::load_item_at()` currently uses `resolve_item_full()` but may discard winner space, winner label, and shadowed candidates. Do not bolt on a second resolution pass just to recover those fields.

## Service migration

### `items.effective`

Make `items.effective` a thin wrapper:

```rust
pub async fn handle(req, _ctx, state) -> Result<Value> {
    let item_ref = CanonicalRef::parse(&req.canonical_ref)?;
    let effective = state.engine.effective_item(EffectiveItemRequest {
        item_ref,
        expected_kind: req.expected_kind,
        project_root: req.project_path.map(PathBuf::from),
    })?;
    Ok(serde_json::to_value(effective)?)
}
```

Request fields:

```rust
pub struct Request {
    pub canonical_ref: String,
    pub project_path: Option<String>,
    pub expected_kind: Option<String>,
}
```

Response should use `composed_value`, not raw parsed content named `composed`.

Temporary compatibility may support both `composed_value` and `composed` on clients, but the service contract should be `composed_value`.

### `client.open`

After `items.effective` works, migrate offline `client.open`:

1. resolve `client_ref` as `client:<id>` through the effective item API/path;
2. require `expected_kind = client`;
3. validate descriptor shape from `composed_value`;
4. validate `serves.kind == surface`;
5. validate requested renderer matches `serves.renderer`;
6. validate `launch.mode`;
7. use `binary_resolver::resolve_bundle_binary_ref` for `cli_exec` binaries;
8. pass the folded effective trust class to binary resolution;
9. exec the binary preserving TTY/stdio.

Do not scan `.ai/clients` directly as the long-term path.

Do not create a hardcoded `tui` branch in `local_verbs.rs` or a new dispatcher lane.

## Surface kind schema

`surface` should be a non-executable kind with top-level resolution:

```yaml
composer: handler:ryeos/core/extends-chain
resolution:
  - step: resolve_extends_chain
    field: extends
    max_depth: 16
composer_config:
  extends_field: extends
  fields:
    - name: layout
      strategy: replace_root_last
    - name: affordances
      strategy: keyed_seq_merge_root_last
      key: id
    - name: bindings
      strategy: keyed_seq_merge_root_last
      key: id
```

If the current transition still uses `commands`, either:

- keep `commands` in the surface composer config as a migration alias, or
- migrate bundled surfaces to `affordances` / `bindings` before enforcing the new field names.

Do not put an `execution:` block on `surface` just to make composition work.

## Client kind schema

`client` should be non-executable with identity composition:

```yaml
composer: handler:ryeos/core/identity
resolution: []
```

The client descriptor is data consumed by `client.open`. It is not a runtime, tool, service handler, or execution target.

## Web UI fit

The daemon-served `/ui` path should use the same substrate:

```text
/ui/api/bootstrap
  -> Engine::effective_item(surface:<id>, expected_kind=surface)
  -> principal/capability snapshot
  -> initial thread/facet/artifact snapshot
  -> session stream URL
```

The browser renderer should never resolve, verify, or compose Rye items.

`/ui` and related routes should remain data-driven daemon routes. WebSocket can later be added as another response mode, not a separate product server.

## Streaming follow-up

The project already has SSE streaming. Do not block effective item work on a stream redesign.

Before adding `/ui/events/session/{session_id}` or WebSocket, introduce a transport-neutral envelope:

```rust
pub struct RouteStreamEnvelope {
    pub id: Option<String>,
    pub event_type: String,
    pub payload: serde_json::Value,
}
```

Then:

```text
stream invoker -> RouteStreamEnvelope -> event_stream response mode -> SSE frames
stream invoker -> RouteStreamEnvelope -> websocket response mode -> WS messages
```

This preserves existing SSE while making WebSocket additive.

## Renderer command runtime follow-up

After effective surfaces work, finish the renderer command runtime:

```text
CommandRegistry = affordances + bindings + policy
InvocationSpec = UiVerb | RyeInvoke
```

The TUI resolves selectors such as `focused.thread` or `selected.artifact`, but the daemon remains authoritative for Rye operations, aliases, services, and permission enforcement.

Do not encode Rye operations as an expanding TUI-specific command enum.

## Verification

Minimum checks for the effective item substrate:

```sh
cargo check -p ryeos-engine -p ryeos-api -p ryeos-tui-core -p ryeos-tui-terminal
cargo test -p ryeos-engine resolution
cargo test -p ryeos-api items_effective
```

Manual checks after daemon wiring:

```sh
ryeosd
ryeos tui --surface surface:ryeos/cockpit/base
ryeos tui --surface surface:ryeos/cockpit/graph
```

Expected:

- explicit missing or untrusted surfaces fail closed;
- `surface:...` resolution goes through `items.effective`;
- graph/trust surfaces show base provenance;
- no local `surface:` filesystem scan occurs;
- no product web sidecar is needed for `/ui`.

## Guardrails

- Keep `run_resolution_pipeline` executable-only unless deliberately changing all launch callers.
- Use `Option<Vec<_>>` for top-level kind `resolution`.
- Use resolution trust classes, not contract trust classes, in effective DTOs.
- Avoid double-resolving to recover provenance.
- Keep `client.open` descriptor-driven through alias/verb/service/offline dispatch.
- Use existing bundle binary resolver rather than a CLI-local binary path resolver.
- Fail clearly for project-space client descriptors that request bundle binary refs until a trusted project-binary story exists.

## End state

```text
Engine effective item substrate
  ├─ items.effective
  ├─ client.open
  ├─ ui.bootstrap
  └─ future inspect/list services

surface:<id>
  signed non-executable item
  verified/composed by engine
  rendered by terminal/web clients

client:<id>
  signed non-executable item
  verified/composed by engine
  consumed by client.open
```

This is the path that keeps TUI, web, CLI, and API aligned with Rye item semantics instead of creating parallel systems.
