<!-- ryeos:signed:2026-11-17T05:02:43Z:b444ad158096d7bf3d8bb4ee891b3ee6a1f4a044c43fb8e72c762d91772f0d69:dvY5nalqsXgx35L/iQ+A29SFjImvAhpRFjrujhXru9q11Ad8ibY0K7cUrmW7RO+AQuTeursvLfbFumakP9kZDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/runtimes
tags: [runtime, directive, llm, callbacks]
version: "1.1.0"
description: Directive runtime execution, callback, and event-backed recovery reference.
---

# Runtime: directive-runtime

Invariant: directive-runtime receives a frozen launch envelope and runs the LLM prompt/tool loop without re-resolving provider trust-sensitive configuration.

It consumes rendered context blocks, resolved provider snapshots, limits, tool inventory, callback environment, thread id, and vault bindings. Tool dispatches are callbacks to the daemon, authenticated by callback and thread-auth tokens and gated by effective caps.

## The turn record

A directive records its conversation as ordered parts. A part is one of `text`,
`image` (an attachment hash with its media type and size), `reasoning` (the
text the provider showed, and its opaque artefact when one must be sent back
verbatim), `tool_call` (`call_id`, `name`, `arguments`, optional artefact) or
`tool_result` (`call_id`, text and image parts, `status` of `ok` or `error`).
The events carry them:

| Event | Payload |
|---|---|
| `cognition_frame` | What the model is given beside the conversation: `frame_seq`, `system` (text parts), `tool_inventory` (the hash of the attachment holding the inventory, repeated in `attachment_refs`), `request_options` (`model`, optional `reasoning`, `sampling`, `max_output_tokens`) and `provider_config` (the hash of the provider configuration in force). A changed frame on resume is recorded as the next `frame_seq`. |
| `cognition_in` | One input: `origin`, `parts`, and optionally `kind`, `ack` and `pinned`; or a turn marker `{ turn, tools_offered? }`. An input too large for one event is split into hash-bound chunks that fold back into one input. |
| `cognition_out` | One assistant turn: `turn`, `parts` in stream order, `finish` (the normalised finish reason, or `interrupted`), optional `usage` and `provider_accounting`. An interrupted turn carries no tool calls. Streaming fragments (`delta`, `tool_use_partial`) stay ephemeral. |
| `tool_call_start` | `operation_id`, `call_id`, `tool`, `turn`, `call_index`, `effective_caps`, and `item_ref` when the call resolves to an item. |
| `tool_call_result` | `operation_id`, `call_id`, `tool`, `status`, `parts` (the exact model-visible result) and `guard` (`result_size_bytes`, `truncated`, optional `truncated_reason`, optional `duplicate_of`). |

The frame, the context inputs and the launch input are appended as one batch
under an idempotency key. A rebuild of the conversation reads these events and
reproduces the provider request without deciding again what the live run
decided.

## Durable tool loop

The final transcript-bearing `cognition_out` event owns an assistant proposal: its
`tool_call` parts, in order.
Each ordered non-lifecycle call receives a stable operation ID derived from the
proposal's emitting thread, positive turn coordinate, and call index. Provider
call IDs remain transcript evidence and do not own retry identity. Before
dispatch, the runtime appends `tool_call_start` with that operation ID; the
daemon binds it to one exact request and one daemon-minted child through the
generic runtime-action intent. Settlement appends one matching
`tool_call_result`.

The directive event braid is its recovery state. On native resume the runtime
folds only the linear continuation path ending at the current thread, retains
the emitting thread for operation-ID derivation, validates proposal/start/result
ordering and identity, and resumes only unfinished calls. Started calls—not
mere proposals—reseed the tool-attempt budget. Child-spawn accounting is
re-derived from those exact starts and the signed tool catalog; no dispatch kind
is added to the generic event schema.

Every replay page must advance its positive chain cursor, stay within the
runtime page ceiling, and charge one process-local budget shared across the
complete continuation-path fold. The combined fold refuses more than 16,384
retained events or 64 MiB of their exact serialized event array; the budget is
not reset at thread boundaries. An invalid cursor or exhausted budget fails
recovery closed rather than truncating history or continuing with partial
authority.

A callback outcome classified as unknown is run-fatal and is never converted
into model-visible tool failure text. The process exits without a result event,
allowing restart to resolve the same retained operation. Known admission
refusals and known child terminals may become ordinary bounded tool results.

Provider token/spend totals come from exact cumulative `thread_usage` events.
A completed live cognition without its same-turn settlement fails recovery
closed. A daemon-proven provider replay may advance the completed-turn
coordinate without fabricating zero usage, and a settlement may be one turn
ahead only at the explicit crash cut before its final cognition event. An
interrupted cognition remains transcript context but is refunded from the
completed-turn counter.

`directive_return` is a provider-facing lifecycle signal, not a dispatchable
tool. It uses the same proposal/start/result visibility but does not consume the
ordinary tool-attempt budget.
