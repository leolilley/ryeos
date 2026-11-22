<!-- ryeos:signed:2026-11-22T02:48:16Z:9777958a22c0f8291a4ff4e329f495322b952e1507edff7ae122a6ffc7b0ac16:y2FisTbpXWtQLmLQQilCLh+pTjX0vsK+Kmqbiz7wOPYcuUDNGq9ZN7jXqARvbPkebwY+MhgeKOr84DM5xVRwCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
| `cognition_frame` | What the model is given beside the conversation: `frame_seq`, `system` (text parts), `tool_inventory` (the hash of the attachment holding the inventory, repeated in `attachment_refs`), `request_options` (`model`, optional `reasoning`, `sampling`, `max_output_tokens`) and `provider_config` (the hash of the provider configuration in force). A changed frame on resume is recorded as the next `frame_seq`, unless the provider entry's `schemas.frame_stability.refuse_change_of` lists a component that changed (`system`, `tools` or `reasoning`), in which case the launch fails with `frame_change_refused` before anything is recorded. |
| `cognition_in` | One input: `origin`, `parts`, and optionally `kind`, `ack` and `pinned`; or a turn marker `{ turn, tools_offered? }`. An input too large for one event is split into hash-bound chunks that fold back into one input. |
| `cognition_out` | One assistant turn: `turn`, `parts` in stream order, `finish` (the normalised finish reason, or `interrupted`), optional `usage`, `provider_accounting` (free JSON: attempt and token counts, including `reasoning_tokens` and `cache_read_tokens`, `cache_write_tokens`, `cache_miss_tokens`, each `null` when the route reports none; `usage.input_tokens` stays the provider's figure, cached tokens included where its usage schema says so) and `malformed_calls` (`{call_index, raw_len, sha256}` for each tool call whose streamed arguments were not valid JSON; `call_index` counts `tool_call` parts). An interrupted turn carries no tool calls. Streaming fragments (`delta`, `tool_use_partial`) stay ephemeral. |
| `tool_call_start` | `operation_id`, `call_id`, `tool`, `turn`, `call_index`, `effective_caps`, and `item_ref` when the call resolves to an item. |
| `tool_call_result` | `operation_id`, `call_id`, `tool`, `status`, `parts` (the exact model-visible result) and `guard` (`result_size_bytes`, `truncated`, optional `truncated_reason`, optional `duplicate_of`, optional `dropped_images`: a `position` and `code` for each tool image that was not used). |

The frame, the context inputs and the launch input are appended as one batch
under an idempotency key. A rebuild of the conversation reads these events and
reproduces the provider request without deciding again what the live run
decided.

## Retried provider calls

A provider failure that the signed execution policy and the directive's
`provider_contact` declaration admit for retry appends one durable
`provider_retry` event before the successor attempt. Its `retry_advance` is the
exact daemon-stamped advancement; its `supersedes` is `{turn, attempt,
output_events}`: the failed attempt and, for a mid-stream cut, how many live
`cognition_out` fragments it had published. Reader rule: a `provider_retry` for
turn T voids every progressive `cognition_out` fragment of turn T published
since that turn's start or the previous `provider_retry`. The fragments are
ephemeral and the retry resends the same messages, so nothing durable records
the abandoned text; only the turn-complete `cognition_out` counts, and resume
folds only that one.

A restart that cuts a provider call is retried the same way. On resume of an
unfinished provider turn the runtime offers the daemon, with every prepare of
that turn, the retry its policy would authorize for that attempt as a
`mid_stream` failure. Stream fragments are live-only and never reach the
record, so the decision's `supersedes.output_events` is 0, and for this decision
the 0 means unknown, not none: the decision's `failure_digest` is the fixed
digest of `ryeos.provider-retry.owner-restart-cut.v1` (no process observed the
failure), which readers use to show "output lost at restart" instead of a
count. A reader voids a retried turn's fragments by position. The daemon stores it only when that
exact attempt was cut by the restart (issued with no recorded answer) and the
sealed launch authority admits another contact: a `provider_contact:
repeatable` directive, a remote transport, and an attempt number within the
sealed `unknown_outcome_retry` ceiling. The resumed run then follows the
ordinary advancement path (one `provider_retry` event, the stored backoff, the
next attempt). Otherwise the resume fails with `provider_attempt_outcome_unknown`
and the provider is not contacted again. A route with no accounting ledger has
no attempt record to prove the cut, so it fails the same way.

Resume is bounded separately: after a hard crash a turn is resumed at most once; planned restarts re-arm.

## Malformed tool calls

A tool call whose streamed arguments are not valid JSON is recorded in the
turn's `malformed_calls` and never dispatched. The runtime records the call's
`tool_call_start` and an error `tool_call_result` that tells the model the
arguments were not valid JSON (byte length and hash) and to resend the call.
The call counts against `limits.tool_calls`. A malformed `directive_return`
gets the same error result and the run continues. Resume rebuilds the decision
from the recorded turn.

## Turns cut at the output limit

When the signed execution config sets `on_output_limit`, a turn the provider
ended at its output limit is recorded with `finish: "length"` instead of failing
the thread. A cut turn with no tool call is followed by a `cognition_in` with
`origin: "harness"` and `kind: "output_limit"` carrying the configured message,
then another turn. The run of trailing `length` turns is read back from the
record on resume and bounded by `max_consecutive`.

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
