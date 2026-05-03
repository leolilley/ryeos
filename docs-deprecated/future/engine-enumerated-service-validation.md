# Engine-Enumerated Service Validation

**Date**: 2026-04-26
**Status**: Future reference — not for current implementation.
**Prerequisite**: Daemon-side `ServiceDescriptor` table is the
single source of truth for built-in services, AND third-party
bundles can ship `kind: service` items.

---

## When to evolve

Today the daemon ships every `kind: service` item itself. The
startup self-check verifies "every built-in descriptor has a
matching signed YAML and a registered handler." That's enough as
long as the descriptor table is the closed set of services.

Evolve to engine-enumerated validation only when one of these
becomes true:

| Condition | Symptom |
|-----------|---------|
| Third-party bundles can ship `kind: service` items | A bundle YAML claims an endpoint nothing handles, or a built-in expects a YAML no bundle ships, with no startup check |
| Plugin / extension surface added | Operators install services dynamically and need verification that everything that loaded actually maps to a handler |
| Operators report endpoint mismatches that survive startup | A service YAML's declared endpoint doesn't match the registered descriptor's endpoint and nothing catches it |

If none of these are true, the closed-set descriptor table is the
right answer. Engine enumeration adds hot-startup-path code that
buys nothing for a built-in-only service set.

---

## What changes

The shape stays:

```rust
pub struct ServiceDescriptor {
    pub service_ref: &'static str,
    pub endpoint: &'static str,
    pub availability: ServiceAvailability,
    pub handler: RawHandlerFn,
}

pub const ALL: &[ServiceDescriptor] = &[ /* built-ins */ ];
```

Startup self-check changes from:

> "for each `descriptor` in `ALL`: resolve its `service_ref` through
> the engine, verify, assert YAML metadata matches descriptor."

to:

> "(a) enumerate every `kind: service` item the engine knows about
> across all loaded bundles. (b) for each item: trust-verify, parse
> metadata, look up its endpoint in the descriptor table, hard-fail
> startup if no descriptor matches. (c) for each descriptor: assert
> at least one item maps to it; hard-fail if a built-in descriptor
> has no provider."

In short: **YAML items are the input set, descriptors are the
target set, both must cover each other exactly.**

---

## Design

### 1. Engine enumeration entry point

Engine exposes a kind-scoped enumeration:

```rust
impl Engine {
    pub fn list_items_by_kind(
        &self,
        kind: &str,
    ) -> Result<Vec<ResolvedItem>>;
}
```

Returns every loaded `kind: service` item from every loaded bundle
root, in source order (system → bundle_roots → state_dir). Each
`ResolvedItem` carries:
- `item_ref` (canonical, e.g. `service:bundle/install`)
- `source_path` (which YAML file)
- `signature_status` (signed by whom; trust verdict)
- `metadata` (parsed YAML body, including `endpoint`,
  `required_caps`, etc.)

The engine already has the kind registry, the trust store, and
the resolver — this is just exposing the iteration.

### 2. Descriptor table lookup

Build a fast lookup at startup:

```rust
let by_endpoint: HashMap<&'static str, &ServiceDescriptor> =
    handlers::ALL.iter().map(|d| (d.endpoint, d)).collect();
let by_service_ref: HashMap<&'static str, &ServiceDescriptor> =
    handlers::ALL.iter().map(|d| (d.service_ref, d)).collect();
```

### 3. Cross-validation pass

```rust
fn validate_service_surface(
    engine: &Engine,
    descriptors: &[ServiceDescriptor],
) -> Result<()> {
    let items = engine.list_items_by_kind("service")?;

    // (a) Every loaded item must map to a descriptor.
    let mut providers_per_descriptor: HashMap<&str, Vec<&Path>> =
        HashMap::new();
    for item in &items {
        // Trust verification is fail-closed.
        if !item.signature_status.is_trusted() {
            bail!(
                "service item {} at {} not trusted: {}",
                item.item_ref,
                item.source_path.display(),
                item.signature_status.reason(),
            );
        }
        let endpoint = item.metadata.endpoint()?;
        let desc = by_endpoint.get(endpoint).ok_or_else(|| {
            anyhow!(
                "service item {} at {} declares endpoint {} \
                 with no registered handler",
                item.item_ref,
                item.source_path.display(),
                endpoint,
            )
        })?;
        // Endpoint matches by construction; double-check service_ref.
        if item.item_ref != desc.service_ref {
            bail!(
                "service item at {}: item_ref {} doesn't match \
                 descriptor service_ref {} for endpoint {}",
                item.source_path.display(),
                item.item_ref,
                desc.service_ref,
                endpoint,
            );
        }
        providers_per_descriptor
            .entry(desc.service_ref)
            .or_default()
            .push(&item.source_path);
    }

    // (b) Every descriptor must have at least one provider item.
    for desc in descriptors {
        if !providers_per_descriptor.contains_key(desc.service_ref) {
            bail!(
                "descriptor for {} has no providing kind:service item",
                desc.service_ref,
            );
        }
    }

    // (c) Optional: warn (not fail) on multiple providers for the
    // same descriptor — could be a bundle accidentally shadowing a
    // built-in. Promote to error if shadowing semantics aren't
    // explicitly designed.

    Ok(())
}
```

### 4. Where it runs

Two options:
- **Daemon startup, post-engine-init**, before serving traffic. Fail
  fast; operator sees the problem.
- **CLI dry-run** (`rye verify --services` or similar), invoked
  manually after bundle install. Same code path; no traffic impact.

Recommend both. Daemon-startup gate is the safety; CLI dry-run is
the operator workflow.

### 5. Shadowing semantics

Multiple bundles registering the same `kind: service` item is the
edge case to design before this lands:

- **Reject:** any duplicate endpoint across loaded items is a
  startup error. Simplest; bundles must coordinate or explicitly
  unbundle a service.
- **First-wins (system → bundles → state):** matches engine
  resolution order for items. Lets a system service be overridden
  by a state-dir item; surprising for daemon-trusted endpoints.
- **System-pinned:** built-in (system-tier) items can't be shadowed
  by bundle items. Bundle items can shadow each other by load
  order.

Recommend system-pinned: built-in services are operator policy and
shouldn't be overridable by an installed bundle's YAML, even if
signed.

---

## Trust model

Engine enumeration is the right surface for trust enforcement
because every item already passes through the engine's trust
verifier on resolve. The validation pass:

- treats unsigned / untrusted-signer items as hard-fail (matches
  the daemon-consumed config trust posture)
- does NOT trust bundle authors to ship arbitrary endpoints — every
  endpoint must map to a descriptor the daemon was built with
- does NOT trust the descriptor table alone — the YAML must exist,
  be signed, and verify

Two checks, both required. Bundle authors can't bypass the
descriptor table. Daemon authors can't ship a descriptor without a
matching YAML.

---

## Migration from closed-set self-check

Drop-in replacement for the existing closed-set self-check. No
descriptor changes; no YAML changes; no handler changes. Just:

1. Implement `Engine::list_items_by_kind`.
2. Replace startup self-check function body with
   `validate_service_surface(&engine, handlers::ALL)`.
3. Add tests:
   - bundle ships YAML for unhandled endpoint → startup fails with
     clear message
   - built-in descriptor with no YAML in any bundle → startup
     fails
   - bundle ships YAML claiming a built-in endpoint → reject per
     shadowing policy
   - all 17 (or N) built-in services pass validation in a default
     install

The descriptor table built for the closed-set case is exactly the
right input shape. Nothing else changes.

---

## Out of scope (further future)

- **Hot-reload validation** — re-run the cross-check when a bundle
  is installed/removed at runtime. Requires the engine to support
  hot-reload of bundle roots, which is its own future work.
- **Capability cross-check** — assert each item's `required_caps`
  matches a per-descriptor expected cap set. Optional; today caps
  are enforced at dispatch, not at startup.
- **Versioned descriptors** — descriptor table evolves as services
  add/remove fields; bundle YAMLs may lag. Versioning the
  descriptor schema is a separate concern; today the YAML schema
  is implicit-by-handler-Request-struct.
- **Graceful degradation** — if a built-in descriptor has no
  provider, run with that endpoint disabled instead of failing
  startup. Probably wrong — silent disabled endpoints are exactly
  the kind of drift this validation exists to catch.

---

## When NOT to evolve

- If the daemon ships every `kind: service` item itself and there's
  no plugin surface, the closed-set check IS the engine-enumerated
  check (the engine just iterates the daemon's own items).
  Implementing this earlier adds enumeration code on the hot
  startup path without changing behavior.

- If `engine.list_items_by_kind` doesn't exist yet and would be the
  first such kind-scoped enumeration entry, building it just for
  service validation is over-investment. Wait until at least one
  other consumer (e.g. routes section validation, runtime kind
  enumeration) needs it.

- If shadowing semantics for service items aren't explicitly
  designed, this validation enforces an undesigned policy. Design
  shadowing first.
