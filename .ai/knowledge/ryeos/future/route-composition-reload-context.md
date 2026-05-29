<!-- rye:signed:2026-05-24T08:05:22Z:797095658beb05f4658602305064e626c8ec4e5713288b7dc396ec6e2c73f2b6:-2CHJJBjbNiI_44E8ATylBLx2faqOmefeA3CCrj9zzzfH4kXEXfEA23K0_ruxuJ9ApEqa_FUvBXxgi3gCKNRDA:4b987fd4e40303ac -->
```yaml
category: ryeos/future
name: route-composition-reload-context
title: Route Composition and Reload Context
entry_type: implementation_guide
version: "1.0.0"
author: amp
created_at: 2026-05-24T00:00:00Z
description: Future implementation path for making daemon route reload use the same composed descriptors, auth verifiers, stream sources, and static providers as startup without introducing UI-specific code into ryeos-api.
tags:
  - route-table
  - composition-root
  - reload
  - extension-registry
  - boundary-hardening
```

# Route Composition and Reload Context

## Purpose

The daemon startup path now builds routes from a composed view of the system:

- API service descriptors plus UI service descriptors.
- API built-in auth verifiers plus UI-registered browser-session auth.
- API built-in event stream sources plus UI-registered session events.
- API static response substrate plus UI-registered web asset provider.

That is the right boundary: `crates/daemon/ryeos-api` stays a generic route
substrate, `crates/daemon/ryeos-ui` owns browser/UI semantics, and
`crates/bin/daemon` is the composition root.

The future route-reload path should preserve that same composition. It should
not rebuild with API-only defaults, and it should not add UI-specific branches
inside `ryeos-api` to recover missing pieces.

## Current state

Startup composition is correct enough for descriptor-instance validation:

1. `crates/bin/daemon` creates `UiState`.
2. The daemon builds a composed descriptor table from API + UI descriptors.
3. The daemon creates API response-mode and route-extension registries.
4. `ryeos_ui::register_extensions` registers UI-specific auth, stream, and
   static asset providers into those generic registries.
5. The route table is compiled from the node-config snapshot using the
   composed registries.

The remaining footgun is route reload. The reload helper should not use
`ResponseModeRegistry::with_builtins()` or `build_route_table_from_snapshot()`
when running inside the daemon, because those are API-only defaults and cannot
know about composition-root extensions.

## Future implementation path

Introduce a small daemon-owned route build context rather than teaching API
about UI:

```rust
pub struct RouteBuildContext {
    pub service_descriptors: &'static [ServiceDescriptor],
    pub response_modes: ResponseModeRegistry,
    pub route_extensions: RouteExtensionRegistry,
}
```

The exact storage type can vary, but the ownership should not:

- `ryeos-api` may define generic registry and builder types.
- `ryeos-ui` may register extension implementations into those registries.
- `ryeosd` owns the composed instance and passes it to startup and reload.

Recommended steps:

1. Add a generic `RouteBuildContext` or equivalent builder type in
   `ryeos-api` that contains only generic route-substrate concepts.
2. Move the daemon's existing startup route composition into a reusable
   `build_route_context(ui_state, descriptors)` helper in the daemon or a
   daemon-local module.
3. Store the composed builder/context on `ApiState`, or store an
   `Arc<dyn RouteTableBuilder>` that closes over the composed registries.
4. Make route reload call the same composed builder that startup uses.
5. Keep API-only helpers available only for API unit tests and explicitly name
   them as API-only so they are not mistaken for daemon composition.
6. Add a regression test that a route requiring an extension auth verifier,
   extension stream source, or extension static provider still compiles after
   reload with the composed context.

## Guardrails

- Do not add `session_events`, `browser_session`, web asset paths, or `UiState`
  branches to `ryeos-api`.
- Do not hardcode launch URLs or route paths in services or clients. Services
  that need a URL should derive it from the current route snapshot or from an
  explicit route-building service contract.
- Do not add descriptor shape/value gates to API or UI binaries as a workaround
  for missing kind-schema expressiveness. Descriptor-instance validation should
  remain engine/kind-schema-owned.
- Do not silently replace registry keys during composition. Duplicate auth
  verifiers, stream sources, static providers, and response modes should fail
  closed at startup.

## When to implement

Implement this when route reload is made user-facing or when the daemon needs
to reload node-config routes without restart. Until then, startup composition is
the authority and API-only builder helpers are acceptable for tests as long as
daemon code does not use them for live reload.

## Success criteria

- Daemon startup and route reload use the same composed descriptors and
  extension registries.
- API-only tests can still build API-only routes without pulling in UI.
- UI routes continue to compile without any UI-specific code in `ryeos-api`.
- A reload cannot drop `browser_session`, `session_events`, or web asset
  providers by accidentally rebuilding with API-only defaults.
