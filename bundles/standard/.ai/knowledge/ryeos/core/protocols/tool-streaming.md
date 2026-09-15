<!-- ryeos:signed:2026-09-06T01:55:11Z:97b249ec4ec274ff68d226d7254f76af64d945710d8526aab74ced2805c37bd5:IqWBDqtg8DBO3JxKXLe7PbG4eK7tmE5Z0kYGaoiDvhR6L5x48AUPBzh7t+QfvAYgObTM9n0R6M2B6941u2PDBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/protocols
tags: [protocol, tool, streaming]
version: "1.1.0"
description: Streaming tool protocol reference.
---

# Protocol: tool_streaming

Invariant: `tool_streaming` emits length-prefixed JSON frames so the daemon can stream tool progress before process exit.

Ordinary `tool:` items select `execution_protocol: protocol:ryeos/core/tool_streaming`
through the Tool kind's signed allowlist. The protocol changes stdout interpretation,
not launch authority: command, argv, environment, timeout, stdin, adjacent source
and external-content mounts come from the same admitted executor plan as any Tool.
`stdin.shape: opaque` preserves signed `config.input_data`; parameter serialization
must be authored through that existing input contract. No callback credentials are
injected, and detached launch is refused.
The admitted result-retention contract must be `full`. `digest_only` streaming
is refused before launch: this protocol has no ephemeral output-delivery surface,
and durable progress must not bypass a signed retention restriction.

Stdout contains a four-byte big-endian length followed by one JSON frame, bounded
by the protocol vocabulary's per-frame ceiling. Sequence starts at zero. Stdout
and stderr frames carry string `data`; the final `exit` frame must carry
`exit_code` and `terminal: true`. Clean EOF must follow. A terminal frame is not
process exit authority: malformed/trailing output, output overflow, cancellation,
timeout or a failing actual process exit cannot become successful completion.

One ordinary process waiter owns deadlines, capture limits and exact cleanup.
Lillux scopes concurrent byte observation under that owner; the decoder must
not occupy a bounded executor-pool slot while a separate waiter is queued
behind it. Observation error/panic interrupts the same wait, and settlement
joins the observer after capture closes. No alternate process runner exists.

Validated frames are projected as `subprocess_output_observed` thread events.
Large data is split at UTF-8 boundaries under the existing exact serialized event
budgets; all pieces of one frame are committed atomically before live publication.
Payloads retain `schema_version`, `launch_owner`, `frame_seq`, `frame_digest`, `kind`,
`exit_code`, `terminal`, `part`, `parts` and `data`. Concatenate `data` by part and
check the canonical frame digest. Null and empty data remain distinct.

The compact terminal result contains `streaming.frame_count`, `data_bytes`,
`exit_code` and `last_observation` (chain/thread coordinates and event hash).
Full output remains in the rooted, replayable event chain, not duplicated into
the terminal result. An observation or terminal-frame marker does not independently
prove task success or authorize candidate publication. Recovery uses the retained
signed protocol and the ordinary executor-plan path with its fresh launch identity.
