<!-- ryeos:signed:2026-11-17T17:48:41Z:a49dcf0c7c29ed9e65e605bc5e6f4f3cdb578dfdec060fc31ad70d5c66a32602:Ej02md7cJWpjIwbTpY4bJsui84YYxEaq2/qCd1NrcLBchk/PcwJJlU9wSfc+00Qlrb6Mfqp3eWOKwH+NKZoXCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
category: ryeos/standard
tags: [models, providers, routing, runtime, security]
version: "1.4.0"
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

A directive execution takes its model identity from the directive runtime's
required `model` reference binding. A caller may bind another directive that
carries the model settings:

```json
{
  "item_ref": "directive:apps/example/agent",
  "ref_bindings": {
    "model": "directive:apps/example/claude"
  }
}
```

The binding is an independently authorized canonical ref. When the caller
gives no `model` binding, the runtime's signed `caller_or_primary_ref` source
binds the executed directive's own canonical ref, so a directive that carries
both behavior and model settings needs no binding. The defaulted ref is
authorized, resolved and trust-checked exactly like an explicit one, and an
explicit binding always wins.

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

The directive bundle's routing table currently sends all tiers through `zen`,
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
`model_providers` catalog (`allowed_spaces: [bundle, node]`, trust
`trusted_bundle`/`trusted_node`) deep-merges the trusted bundle entry with any
node-space entry of the same id.

### Changing a provider's endpoint

Put an operator-signed file at
`<app-root>/.ai/config/ryeos-runtime/model-providers/<id>.yaml` and sign it with
`ryeos sign .ai/config/ryeos-runtime/model-providers/<id>.yaml` from the app
root (the operator key is scoped to the project and node spaces). The merge is
deep, so the file carries only the changed keys plus the merge keys, for example:

```yaml
category: ryeos-runtime/model-providers
transport:
  kind: remote_http
  base_url: https://llm.internal.example/v1
```

The next run picks it up; no restart. Project-space files are not an option
because the signed launch contract refuses them: a project could otherwise
redirect the provider credential to an arbitrary host.

`base_url` is not interpreted the same way for every family:

- `anthropic_messages`: the full request URL (`https://api.anthropic.com/v1/messages`).
  The shipped config sets `extra.stream_url: ""`, which means "use `base_url` as is".
- `chat_completions` families: a base. `/chat/completions` is appended unless
  `extra.stream_url` is set; a non-empty `stream_url` is appended instead (one
  `/` between them) and an empty one appends nothing.
- In every family `{model}` in `base_url` is replaced by the model name first
  (Gemini-style `.../models/{model}:streamGenerateContent`).

The credential check URL is separate: `extra.setup.validation.url` (for example
`https://api.anthropic.com/v1/models`) must share the `base_url` origin and
use HTTPS when it sends a credential, so moving the endpoint to another host
requires overriding `extra.setup.validation.url` in the same file. Keys not
named in the node file keep their bundle values.

`ryeos validate` on a directive reports the result without running it:
`runtime_preparation.runtime_facts` carries `provider_id`, `model_name`,
`request_url` (the URL after the joining and `{model}` rules above; absent for
admitted local workers) and `config_sources` (the root label of each merged
bundle and node entry).

An admitted-local-worker provider contributes its exact signed worker as an
opaque prepared execution dependency. Provider selection remains wholly in the
directive launch preparer: the executor does not branch on provider, model,
worker kind, or profile name. A workload that needs multiple exact local
profiles authors multiple provider configs and concrete signed workers. Shared
implementation source and realization pins may remain equal, while any signed
worker contributor difference moves the dependency resolution, effective
program, persistent-session pool identity, capsule, restart authority, replay
coordinate, and comparison evidence.

`ryeos validate` invokes the same bounded signed launch preparer and exact
cached admission path used by execution. Its read-only projection reports the
selected runtime, path-free ref-binding records, opaque execution dependencies
keyed by their preparer-owned names, canonical refs and as-launched resolution
digests, source readiness, exact current external-content consumer/publisher
bindings, node session-policy eligibility, and the preparer's safe runtime facts (provider, model, effective request URL). Missing or released content is
reported as not ready; corrupt authority fails closed. Validation never
serializes opaque runtime data or secrets and never starts the selected
workload, creates a session or lease, reserves capacity, publishes CAS state,
or creates an execution thread. Live execution repeats authoritative admission
so a later binding release or policy change cannot borrow validation authority.

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

## Reasoning capture and replay

What a provider returns as reasoning, and what it needs back on the next
request, is declared on the entry under `schemas.reasoning`; the runtime never
names a provider field. An entry that declares neither reads no reasoning from
its stream and sends none back. Capture and replay apply to `delta_merge`
streams only.

`capture` is a list of rules, each with an `id` of 1 to 32 lowercase letters,
digits or `_`:

| Kind | Reads |
|---|---|
| `text_delta` | `fields`: string delta keys that are the visible reasoning text; the first one present in a chunk is taken. |
| `blocks` | `field`: an array in the delta whose items assemble into one opaque artefact per block. `merge_keys` are string keys concatenated across the items of one block, every other key keeps its latest value. A block completes when `close_key` is present with a non-empty value, or on arrival when its `type_key` value (default `type`) is in `whole_types`. `group_key` separates interleaved blocks, `text_key` also emits that key's string as reasoning text. A block still open when the stream finishes, or displaced by one of another type, is dropped with the warning `reasoning_artefact_incomplete` and is never replayed. |
| `tool_call_field` | `field`: a key riding on streamed tool-call entries (`merge` is `last` or `object_merge`); the value is stored on the call it belongs to. At most one such rule. |

A completed block is recorded as its own reasoning part with only an artefact;
visible reasoning text stays in adjacent text parts. Artefact bytes count toward
`max_stream_output_bytes_per_turn` together with the text, so a provider that
returns its thinking both as text and inside a signed block spends the cap twice
instead of growing the recorded turn past it. An entry that wants one copy
declares only the `blocks` rule and no `text_delta` for the same text.

`replay` has an optional `compat_group` and a list of `placements`. An artefact
is stored in an envelope `{capture, group, value}` and replays only when the
active entry declares the same `compat_group`, so a model switch never sends one
provider's signatures to another. The group is required once any rule stores an
artefact.

Each placement has `from` (a capture id), `into` (`message_field: <key>` for the
joined text or the array of block values, or `tool_call_field: <key>` for the
value set on the rendered call), a `scope` and an `on_missing`:

- `scope` is `all` (every assistant message), `tool_call_turns` (messages with a
  tool call) or `open_loop` (messages after the last user message).
- A message owes a placement when it is in scope and proposes a tool call.
  `on_missing` is then `omit` (send nothing), `refuse` (fail the turn with
  `reasoning_artefact_missing`, checked when the turn completes, before any tool
  it proposed is dispatched, and again when a request is built from history) or
  `{fill: <json>}` (send the literal; on a tool-call field it goes on the first
  call). Text placements are never owed.

Documented shapes. Entries for specific models must be confirmed against the
live gateway before they ship; the Claude, Gemini and gpt-oss rows are
unconfirmed.

```yaml
# Text echoed back on every assistant message (DeepSeek, Z.AI shape).
reasoning:
  capture:
    - {id: text, kind: text_delta, fields: [reasoning_content]}
  replay:
    placements:
      - {from: text, into: {message_field: reasoning_content}, scope: all}

# Signed thinking blocks required for the current tool loop (unconfirmed).
reasoning:
  capture:
    - {id: blocks, kind: blocks, field: thinking_blocks, merge_keys: [thinking],
       close_key: signature, whole_types: [redacted_thinking]}
  replay:
    compat_group: claude-thinking-1
    placements:
      - from: blocks
        into: {message_field: thinking_blocks}
        scope: open_loop
        on_missing: refuse

# A signature riding on the first tool call of a batch (unconfirmed).
reasoning:
  capture:
    - {id: sig, kind: tool_call_field, field: extra_content, merge: object_merge}
  replay:
    compat_group: gemini-sig-1
    placements:
      - from: sig
        into: {tool_call_field: extra_content}
        scope: open_loop
        on_missing: refuse

# Reasoning text sent back only inside the open loop (unconfirmed).
reasoning:
  capture:
    - {id: text, kind: text_delta, fields: [reasoning]}
  replay:
    placements:
      - {from: text, into: {message_field: reasoning}, scope: open_loop}
```

Reasoning effort and thinking budget use the `mode` and `effort` mappings
above. A model that rejects an effort value maps it to one it accepts in the
per-model value map, and an entry for a model that rejects disabling thinking
declares no `mode.disabled`. Reasoning tokens are accounted by the usage schema:
`reasoning_tokens_path` reads them and `reasoning_included_in_output` says
whether `completion_tokens` already contains them (true is the usual shape).
A reasoning count above the output count while that is declared is recorded as
a contract anomaly. List `reasoning_tokens` among a tariff's covered dimensions
only when the provider bills them separately from output.

An entry may also declare which parts of the frame it refuses to see change
mid-conversation:

```yaml
schemas:
  frame_stability:
    refuse_change_of: [system, tools, reasoning]
```

A resume or continuation whose launch changes a listed component fails with
`frame_change_refused`, naming the component and the recorded `frame_seq`,
before anything is recorded. Unlisted changes record the next frame.

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

The directive bundle ships signed provider configs for:

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
