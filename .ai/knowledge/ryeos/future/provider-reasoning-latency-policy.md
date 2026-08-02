<!-- ryeos:signed:2026-08-02T03:04:32Z:c86ced1618b42771dcfd6f3a55a64f24c06e8dad689dede9d09a260fc8da978d:nW+178UFl+Xy19g/xcLDMqEjNA4DwugU5JxNDxhupAAK9QEh4TXN5fgvtxqq7oMdraceLxT2tHNwvB52hj75Dw==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
```yaml
category: ryeos/future
name: provider-reasoning-latency-policy
title: Signed Provider Reasoning and Latency Policy
description: Future provider-neutral controls for explicit reasoning mode without hidden prompt classification or provider-specific app workarounds
entry_type: design
version: "0.1.0"
```

# Signed Provider Reasoning and Latency Policy

## Status

Future design. RyeOS currently renders signed provider templates and profile
`body_extra`, but a directive model binding exposes only temperature and seed.
DeepSeek V4 therefore receives no explicit thinking control and uses the
provider default: thinking enabled with high effort.

DeepSeek's official API supports `thinking.type` values `enabled` and
`disabled`, plus `reasoning_effort` values `high` and `max`. Its documentation
also requires reasoning content to be replayed after assistant tool calls in
thinking mode. The current directive runtime preserves that replay contract.

References:

- https://api-docs.deepseek.com/guides/thinking_mode
- https://api-docs.deepseek.com/api/create-chat-completion

## Decision

Add a provider-neutral, signed reasoning policy to model bindings. The policy
must be resolved and sealed during launch preparation, translated only through
provider-declared schema paths, included in provider-config/request digests,
and recorded as a launch fact.

The initial shape should express:

- mode: provider default, enabled, or disabled;
- effort: a provider-declared enum such as high or max;
- an optional signed route default;
- whether tools require hidden reasoning replay.

Provider descriptors declare how these values map to wire fields. Runtimes must
not switch on a provider ID or model name to invent body fields. An unsupported
value fails before spend reservation and request submission.

## Non-goals

- no keyword or greeting classifier;
- no canned-answer path;
- no automatic switch based on prompt length;
- no silent global change to existing model semantics;
- no loss of reasoning replay required for tool-call continuity;
- no exposure of reasoning content in progress events or ordinary logs.

## Why this precedes workers

A managed worker can remove process/bootstrap and first-connection costs, but
cannot remove provider inference. A small direct DeepSeek V4 Pro sample on 2
August 2026 measured first text at 1.59–2.32 seconds with thinking enabled and
0.86–0.88 seconds with thinking disabled for `hello`. This is not a production
policy decision or a complex-workload benchmark; it establishes that explicit
reasoning mode is a measurable provider-side latency lever.

## Pull-forward gate

Implement after provider request-shape telemetry is available and a downstream
workload can declare the intended quality/latency policy explicitly. Validate
at least simple chat, one-tool analysis, and multi-tool analysis with identical
provider-ready inputs. Compare latency, tool-call correctness, answer quality,
token usage, and failures before selecting any default.
