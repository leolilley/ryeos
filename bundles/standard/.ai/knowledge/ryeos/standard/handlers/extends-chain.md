<!-- ryeos:signed:2026-06-22T02:50:09Z:1aa20e6d2868a7f0cb53a961008e810b9fb2383049daadadc4403388044a56fb:GvflgBqAKi2oR/uZUkHrQOxVy1/fWqF+HqDNOR7L1aITHYe3iSIXGuXLYzjuVhNOwnButdXY9nwL5B5Ps57XDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/handlers
tags: [handler, directive, extends]
version: "1.0.0"
description: Extends-chain composer reference.
---

# Handler: extends-chain

Invariant: extends-chain resolves directive inheritance into one effective directive while preventing children from widening their parent's capability requirements.

Key merge strategies:

- `root_verbatim` for directive body: the root body is the executed prompt.
- `narrow_requires_capabilities` for `requires.capabilities`: `declared` (self-asserted action authority) narrows by dropping caps the parent lacks; `manifest` (runtime authority) fails compose if the child widens beyond the parent.
- `dict_merge_string_seq_root_last` for context: context arrays are merged by position with root-last ordering.

The handler derives `policy_facts.effective_caps` from `requires.capabilities.declared` for daemon-side callback enforcement.
