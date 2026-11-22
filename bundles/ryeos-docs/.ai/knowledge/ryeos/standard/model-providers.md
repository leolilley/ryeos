<!-- ryeos:signed:2026-11-22T15:50:37Z:ad3093f6ed739558a5954c455b6991e8d21eeddcc503f200997bb4fe18dc3250:bg9kmsx42T4ZY17ln0mBPfEOzvAJ2KWB1HmTOE3dE6ENBcvxYWG0XfxI5gycXVlnJyCuOJOYrnh9SMQzez7gBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

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
  `fill` (send the literal in the placement's sibling `fill` key; on a
  tool-call field it goes on the first call). Text placements are never owed.

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

## Image input

A provider route can answer 200 and silently drop an image a model cannot read,
so image support is declared, per model, never learned from an error. `image_input`
sits at the top level of the provider entry and, like any profile field, a
profile's value replaces the entry's whole.

```yaml
image_input:
  unsupported: refuse          # refuse | text_note, for a model with no `accepts`
  accepts:                     # absent: the model takes no images
    media_types: [image/png, image/jpeg, image/gif, image/webp]
    max_bytes_per_image: 3145728
    max_images_per_request: 20
    text_part_template:  {type: text, text: "{text}"}
    image_part_template: {type: image_url, image_url: {url: "{data_url}"}}
    tool_result_note: "[{count} image(s) follow in the next message]"
    deferred_label:   "Image {index} from tool result {call_id}:"
    omitted_note:     "[earlier image omitted: {media_type} {width}x{height}]"
    cost: {base_tokens: 0, pixels_per_token: 750, max_tokens: 1600, unknown_dimensions_tokens: 1600}
```

The part templates substitute whole-string placeholders (`{text}`; `{data_url}`,
`{data_b64}`, `{media_type}`, `{width}`, `{height}`). The three notes use named
placeholders and an unknown one fails config validation.

A model with no `accepts` and `unsupported: refuse` (the default) gets an error
tool result, `image_input_unsupported`, and nothing is stored; `text_note` keeps
the result text and notes the image. The record keeps an image inside the tool
result that returned it. When a request is built, the images of each run of
tool messages move into one user message placed after the tool results, the
same for every model, and each tool message keeps a note saying so. A request
carries only the newest `max_images_per_request` images; older ones become the
`omitted_note` text in place. The estimate counts the same images by `cost`.
A history that holds an image the entry cannot send fails the attempt with
`image_input_unsupported` before any provider request. An image larger than the
entry's `max_bytes_per_image` (or too large to read back in one callback frame)
is not read back: it stands in the conversation as `[image not used:
image_oversize]` and the run goes on. Images the next request carries are held
in memory and are never evicted while that request is built.

## Cache marks

Some routes cache a prompt prefix only where the request marks it. `cache_marks`
sits at the top level of the provider entry, applies to Chat Completions entries
only, and a profile's value replaces the entry's whole. An entry without it sends
no mark.

```yaml
cache_marks:
  mark: {cache_control: {type: ephemeral}}        # keys merged into a content part
  text_part_template: {type: text, text: "{text}"} # how a string content becomes a part
  max_marks: 4                                      # 1 to 4, the route's limit
  positions: [system, newest, previous_tail]        # priority order
```

The marks are written after the request's messages are final, so they land only
on bytes the request really sends. `system` is the system message, `newest` the
last message, and `previous_tail` the message just before the last assistant
message. When the last request ended on that message (the usual case: a tool
result or a user input), it is the message the previous request marked as its
newest, so the cache entry written last turn is read at an exact breakpoint. A
user message that follows the tool results (a wrap-up input or an operator
steer) takes the newest mark instead, and the previous tail then marks the
message before the assistant, not the previous newest. A position with no
message (no earlier assistant turn yet) costs no mark, and positions past
`max_marks` are dropped. The mark goes on the last content part of the message
and never on an assistant message. With `cache_marks` declared, every system,
user and tool message with text renders as a part array on every turn, marked or
not, so a mark moving to a newer message leaves the older message's bytes equal
apart from the mark key. A selected message with empty content, or content that
does not end in an object part, takes no mark.

Validation (of the entry and of every profile resolved from it) refuses `cache_marks` on a route that renames the system or assistant
role, extracts the system message, uses another content key or tool role, or does not wrap
tool results directly. A tariff on a marked route must cover `cache_read_tokens`
and `cache_write_tokens`, because input and output are no longer the complete
billable set.

Marks do not change what the run costs where no tariff exists: cached tokens are
recorded, and priced only where a tariff covers their dimension. A change to the
tool list, the system prompt or any earlier message starts a new cache entry, so
the next request pays one full miss.

## Request rejections

A provider can refuse a request before it streams anything: the status alone is
too coarse to say why (a bare 400 is also a malformed request). `request_rejections`
sits at the top level of the provider entry, and a profile's value replaces the
entry's whole. An entry without it reports every refusal as the generic
`directive_failed`.

```yaml
request_rejections:
  - code: provider_context_length_exceeded
    statuses: [400, 413]
    body_contains_any: ["context length", "too long"]
```

A row matches a refused response when its status is listed and the response body
(the first 4 KiB) contains any phrase, compared without regard to case; the
first matching row wins. A match makes the thread fail with that `code`, and a
refusal a row names is never retried, whatever `retry_status_codes` says. A
refusal no row matches stays `directive_failed` and follows the ordinary retry
rules (including the `Retry-After` handling described in `execution-config`). Only a response that carried a status can match, so an uncertain outcome
(a timeout, a cut stream) is never reclassified.

`code` is an opaque token chosen by the entry: lowercase letters, digits and
underscores, at most 64 bytes, and it must start with `provider_`; a code the
runtime reports itself (for example `provider_outcome_uncertain`) is reserved and
refused. The runtime
gives no code a meaning. A parent graph that follows the directive reads it as
`state._outcome.failure_code`, and the directive's own `provider_error` hooks
(observation only) receive it as `event.code` beside `event.status` and
`event.turn`. At most 16 rows of at most 16 phrases each; statuses are 400 to 599.

The runtime reads `request_rejections` with `deny_unknown_fields`, so a runtime
payload older than a bundle config that declares it refuses the config. Refresh
the runtime payload and the bundle's provider configs together.

## Malformed replies

Some models sometimes write a tool call as plain text in their own markup instead
of making a structured call. The reply then has text and no tool calls, and
without a policy the run would finish with that text as its result.
`malformed_replies` sits at the top level of the provider entry, and a profile's
value replaces the entry's whole; an entry without it finishes as before.

```yaml
malformed_replies:
  markers: ["<|channel|>", "<|message|>"]
  message: "Your last reply was not a valid tool call. Make the call again as a structured tool call."
  max_consecutive: 3
```

A reply with no structured tool call whose text parts (not its reasoning) contain
any marker, compared exactly and case-sensitively, is malformed. The runtime
never parses or runs a call written as text. It drops the reply from the
conversation sent next, records the entry's `message` as a harness input of kind
`malformed_reply_turn_<turn>_streak_<n>` (so the record names the turn it
corrects), and takes another turn. After `max_consecutive` corrections in a row,
or when no turn is left, the run fails with `malformed_reply_retries_exhausted`
and the failure names the marker it matched; the reply is never the result. A
genuine answer that happens to contain a marker is retried the same way, so
choose markers that only the malformed form contains. Every malformed reply is
logged with its turn, streak position and marker.

All three fields are required: 1 to 16 markers of 1 to 128 bytes (no control
characters, none a substring of another), a message of 1 to 4096 bytes, and
`max_consecutive` from 1 to 8. The runtime reads the strings as opaque data and
gives none a meaning. It reads the field with `deny_unknown_fields`, so refresh
the runtime payload and the bundle's provider configs together.

## Provider cache usage

A signed streaming usage schema may map cache-read, cache-miss, and cache-write
token paths. When it declares `cache_partition_of_input: true`, both read and
miss paths are required and RyeOS validates that their checked sum equals the
reported input-token total. Cache miss is a distinct accounting dimension; it
is never relabelled as cache write.

A usage schema also reads cache counts that sit inside the input count. It
declares `cache_read_tokens_path` and `cache_write_tokens_path` and sets
`cache_included_in_input: true`; a report whose read plus write exceeds the
input count is then an anomaly, so the turn's token counts are invalid rather
than silently over-counted. It cannot be combined with `cache_partition_of_input`, and a tariff covering `input_tokens` together with a
cache dimension is refused because it would bill the cached part twice. The
counts are recorded on the turn as `provider_accounting.cache_read_tokens`,
`cache_write_tokens` and `cache_miss_tokens`; a path the schema does not declare
is recorded as `null`, never guessed from a field name.

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
