<!-- ryeos:signed:2026-06-11T21:03:05Z:26b71936e7faf2191d1b51c04e55c75cc8424b6aa030562b847f4034a38f9b92:rH3CvXfKSPXyimkpkK7Zqpo2nf9pzA1/zb8QSoMRGS2hWFuLZVeFLnccOO3nyvU6O1d/eqqs2nGpJFwxQwbXBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
category: ryeos/standard
tags: [models, providers, routing, runtime, security]
version: "1.0.0"
description: >
  Runtime model provider resolution: directive model settings, routing
  tiers, provider configs, frozen provider snapshots, and adding new
  providers.
---

# Model Providers

Invariant: a directive run resolves one coherent provider/model pair and
freezes the provider config before runtime execution begins.

## Resolution order

Provider selection is implemented in
`ryeos-runtime/src/model_resolution.rs:658-729`:

1. If a directive names `model.name`, it must also name
   `model.provider` and `model.context_window`. This keeps provider and
   model coherent.
2. Otherwise the directive's tier selects a row from
   `config:ryeos-runtime/model_routing`.
3. The selected provider id loads
   `config:ryeos-runtime/model-providers/<provider>`.
4. Provider defaults and model-profile overrides are merged into the
   concrete HTTP schema, auth header, streaming mode, and pricing.

The standard routing table currently sends all tiers through `zen`,
which profiles Claude, GPT, Gemini, and open-weight model families.

## Frozen provider snapshots

Daemon preflight resolves routing and provider config before launch and
returns a `ResolvedProviderSnapshot` (`model_resolution.rs:733-813`).
The snapshot includes the selected provider id, model name, context
window, config hash, and fully resolved provider schema. Freezing this
data avoids a time-of-check/time-of-use split between daemon preflight
and runtime HTTP calls.

Provider configs control outbound URLs and auth env vars, so project-root
provider contributions are rejected unless the explicit trust override is
enabled (`model_resolution.rs:770-783`).

## Active configs

The standard bundle ships signed provider configs for:

- `zen` — primary gateway and default route target.
- `anthropic` — direct Anthropic Messages API.
- `openai` — direct OpenAI Chat Completions API.

Add provider configs only when a routing entry, directive, fixture, or
test selects that provider.

## Adding a provider

To add a provider:

1. Add a signed YAML under
   `config/ryeos-runtime/model-providers/<provider>.yaml`.
2. Declare the family, auth header, request/response schemas,
   streaming mode, and pricing defaults.
3. Add model-profile overrides when one endpoint serves multiple wire
   formats.
4. Point a `model_routing.yaml` tier at the provider or use it from a
   directive with explicit `model.provider`, `model.name`, and
   `model.context_window`.
5. Add an e2e or runtime test covering the provider id.
