<!-- ryeos:signed:2026-10-17T05:23:22Z:1f644776ebd06da9d3e40611af46574b9306e7b2830589e875278fb22fca369e:lEqploWLF7uv35k3elNeEAxViKb0o445I53WhFf0Y+YwULTq0ZXU9Si+Gq5QvJgDkSE77rMcFEJFIJW5Vn+WBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/handlers
tags: [handler, directive, extends]
version: "1.0.0"
description: Extends-chain composer reference.
---

# Handler: extends-chain

The core bundle ships this handler; this reference lives in the ryeos-docs bundle.

Invariant: extends-chain resolves directive inheritance into one effective directive while preventing children from widening their parent's capability requirements.

Key merge strategies:

- `root_verbatim` for directive body: the root body is the executed prompt.
- `narrow_requires_capabilities` for `requires.capabilities`: `declared` (self-asserted action authority) narrows by dropping caps the parent lacks; `manifest` (runtime authority) fails compose if the child widens beyond the parent.
- `dict_merge_string_seq_root_last` for context: context arrays are merged by position with root-last ordering.

The handler derives `policy_facts.effective_caps` from `requires.capabilities.declared` for daemon-side callback enforcement.
