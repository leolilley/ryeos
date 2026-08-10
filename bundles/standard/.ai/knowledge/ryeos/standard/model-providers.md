<!-- ryeos:signed:2026-08-10T03:16:08Z:85a5f7302c6516131838d76c2f95f51261b78fd093510285b019936fd083bc97:TeJPmfrI+3jtF5wpWugNkQXIQa+njdUiRrJFZesC6/0aGdntZuquKEBz0AaWwyEQlHH61XTDsJcyTENInyedAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
category: ryeos/standard
tags: [models, providers, routing, runtime, security]
version: "1.3.0"
description: >
  Directive-owned model/provider launch preparation, routing tiers,
  provider configs, frozen runtime data, and adding new providers.
---

# Model Providers

Invariant: a directive run resolves one coherent provider/model pair and
freezes the provider config before runtime execution begins. The generic
executor only resolves named reference bindings and enforces the selected
runtime's signed launch contract; it does not interpret model or provider
semantics.

## Execution identity

A directive execution supplies its model identity through the directive
runtime's required `model` reference binding:

```json
{
  "item_ref": "directive:apps/example/agent",
  "ref_bindings": {
    "model": "directive:apps/example/claude"
  }
}
```

The binding is an independently authorized canonical ref. If one directive
supplies both behavior and model settings, its canonical ref is repeated
explicitly in the `model` slot.

## Resolution order

Provider selection is owned by the pure directive launch preparer and the
directive kind definition:

- `ryeos-handler-bins/src/directive_launch.rs`
- `crates/kinds/directive-definition/src/lib.rs`

1. The preparer reads model settings from the resolved `model` binding. If it
   names `model.name`, it must also name
   `model.provider` and `model.context_window`. This keeps provider and
   model coherent.
2. Otherwise its tier selects a row from
   `config:ryeos-runtime/model_routing`.
3. The selected provider id loads
   `config:ryeos-runtime/model-providers/<provider>`.
4. Provider defaults and model-profile overrides are merged into the
   concrete HTTP schema, auth header, streaming mode, and pricing.

The standard routing table currently sends all tiers through `zen`,
which profiles Claude, GPT, Gemini, and open-weight model families.

## Launch preparation and frozen runtime data

The signed directive runtime descriptor selects
`handler:ryeos/standard/directive-launch` as its launch preparer. The generic
launch machinery supplies verified, path-free primary/binding/config
snapshots. The handler returns opaque `provider_snapshot` runtime data,
symbolic secret requirements, and safe runtime facts. The exact prepared
result is audited and transferred to the scheduled runtime spawn.

The directive runtime requires and consumes `provider_snapshot`. It rejects a
missing snapshot and any unknown runtime-data key. The snapshot includes the
selected provider id, model name, context window, sampling and reasoning
policies, config hash, source digests, and fully resolved provider schema.
Freezing this data avoids a
time-of-check/time-of-use split between authoritative launch preparation and
runtime HTTP calls.

Provider configs control outbound URLs and auth env vars, so project-root
provider contributions are excluded by the signed launch contract. The
`model_providers` catalog accepts trusted bundle entries only.

## Provider-neutral reasoning policy

A signed model binding may optionally select `model.reasoning.mode` as
`provider_default`, `enabled`, or `disabled`, plus a provider-declared effort.
The selected provider config must declare the corresponding object paths and
wire values under `schemas.reasoning`. The directive runtime follows that data;
it never switches on a provider ID or model name to mutate the request.

Absence preserves the provider's existing request shape. Unsupported values,
`disabled` combined with effort, and invalid provider mappings fail during
launch preparation. The resolved policy is frozen in the provider snapshot and
recorded as a runtime fact before the exact provider request is serialized and
digested.

## Provider cache usage

A signed streaming usage schema may map cache-read, cache-miss, and cache-write
token paths. When it declares `cache_partition_of_input: true`, both read and
miss paths are required and RyeOS validates that their checked sum equals the
reported input-token total. Cache miss is a distinct accounting dimension; it
is never relabelled as cache write.

Pricing may declare paired cache-read/cache-miss rates for operational budgets,
while the signed spend tariff covers the exact billable dimensions used for
authoritative settlement. Missing or contradictory provider cache metadata
fails closed for a tariff that requires it. Request equality alone is never
evidence of a provider-side hit.

## Active configs

The standard bundle ships signed provider configs for:

- `zen` — primary gateway and default route target.
- `anthropic` — direct Anthropic Messages API.
- `openai` — direct OpenAI Chat Completions API.
- `openrouter` — multi-provider OpenAI-compatible gateway.
- `deepseek` — direct DeepSeek OpenAI-compatible Chat Completions API.
- `zai` — direct Z.AI OpenAI-compatible Chat Completions API.
- `local-openai` — local or offline OpenAI-compatible inference.

Add provider configs only when a routing entry, directive, fixture, or
test selects that provider.

## Adding a provider

To add a provider:

1. Add a signed YAML under
   `config/ryeos-runtime/model-providers/<provider>.yaml`.
2. Declare the family, auth header, request/response schemas,
   streaming mode, usage/cache metadata, and pricing defaults.
3. Add model-profile overrides when one endpoint serves multiple wire
   formats.
4. Point a `model_routing.yaml` tier at the provider or use it from a
   directive with explicit `model.provider`, `model.name`, and
   `model.context_window`.
5. Verify the provider through the directive launch-preparation and runtime
   paths.
