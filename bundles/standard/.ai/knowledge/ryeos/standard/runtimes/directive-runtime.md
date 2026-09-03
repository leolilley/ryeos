<!-- ryeos:signed:2026-09-01T02:56:15Z:5c413fa9556b8c7eb74cc53539c143e2c88c887a88257cd9662e355740d28ddb:EOolqKTc6taxw73NcF083phd6xx3qtm2OYOkE7qNO1otQ1XSbOHjlo7qnAZwoT0tGlePV6O2Jdz2TFy3zEWXDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/runtimes
tags: [runtime, directive, llm, callbacks]
version: "1.1.0"
description: Directive runtime execution, callback, and event-backed recovery reference.
---

# Runtime: directive-runtime

Invariant: directive-runtime receives a frozen launch envelope and runs the LLM prompt/tool loop without re-resolving provider trust-sensitive configuration.

It consumes rendered context blocks, resolved provider snapshots, limits, tool inventory, callback environment, thread id, and vault bindings. Tool dispatches are callbacks to the daemon, authenticated by callback and thread-auth tokens and gated by effective caps.

## Durable tool loop

The final transcript-bearing `cognition_out` event owns an assistant proposal.
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
