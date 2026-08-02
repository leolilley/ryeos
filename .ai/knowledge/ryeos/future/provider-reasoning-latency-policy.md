<!-- ryeos:signed:2026-08-02T07:16:51Z:6455a73cddf0dcc4789d9c052634558e37b57bf06c2866925131fec30cabfcb1:qVCoQpf7P2v8BxPSg23I+re4KW+eTRk3jktivMX8B6wrbrtPN/ojJZ5S6/LyaNaZK9DkIGJRBB3K/DOHWOpvDA==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
```yaml
category: ryeos/future
name: provider-reasoning-latency-policy
title: Signed Provider Reasoning and Latency Policy
description: Provider-neutral signed reasoning controls, their authority boundaries, and deferred latency-policy work
entry_type: design
version: "0.2.0"
```

# Signed Provider Reasoning and Latency Policy

## Status

The provider-neutral mechanism was pulled forward on 2 August 2026. A signed
model binding may now select reasoning mode and effort, while a signed provider
descriptor owns the corresponding wire paths and values. Absence of the model
policy preserves the previous provider request shape and provider default.

Route defaults, automatic policy selection, and workload-specific defaults
remain deferred. No downstream application is changed merely by shipping the
mechanism.

DeepSeek's official API supports `thinking.type` values `enabled` and
`disabled`, plus `reasoning_effort` values `high` and `max`. Its documentation
also requires reasoning content to be replayed after assistant tool calls in
thinking mode. The current directive runtime preserves that replay contract.

References:

- https://api-docs.deepseek.com/guides/thinking_mode
- https://api-docs.deepseek.com/api/create-chat-completion

## Implemented contract

A model directive may declare:

```yaml
model:
  provider: deepseek
  name: deepseek-v4-pro
  context_window: 1000000
  reasoning:
    mode: provider_default # provider_default | enabled | disabled
    effort: high           # optional provider-declared value
```

The signed provider descriptor declares the mapping:

```yaml
schemas:
  reasoning:
    mode:
      path: thinking.type
      values: {enabled: enabled, disabled: disabled}
    effort:
      path: reasoning_effort
      values: {high: high, max: max}
```

The runtime does not inspect provider IDs, model names, prompt text, or
application identity. Launch preparation resolves the policy against the
selected provider mapping and fails closed before spend or provider execution
when a mode or effort is unsupported. It seals the policy in
`provider_snapshot` and the `reasoning` runtime fact. Request preparation
applies the declared mapping before serializing the immutable body and deriving
its body and request digests.

`disabled` cannot be combined with an effort. `provider_default` does not write
a mode field, although it may carry a mapped effort. A missing policy writes no
reasoning fields and therefore retains exact backward-compatible semantics.

The existing hidden-reasoning replay behavior remains unchanged. Reasoning
content needed after an assistant tool call stays in the provider transcript;
it is not converted into visible assistant text or progress copy.

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

## Deferred policy work

Do not select a downstream default from simple-message latency alone. Validate
at least simple chat, one-tool analysis, and multi-tool analysis with identical
provider-ready inputs. Compare latency, tool-call correctness, answer quality,
token usage, and failures before changing a model binding.

A future signed route default may be useful, but it must remain distinguishable
from absence so upgrades cannot silently change existing model semantics.
Automatic prompt classification remains a non-goal.
