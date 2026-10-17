<!-- ryeos:signed:2026-10-17T01:32:58Z:fa48c91c371c9217566792f7cab19e746fcd2dab5da579dbd5f1ee7bf588cb38:V7Q5+VzqoL8D/jf90yKRi81Uwn7Fjz6DSJ16BXhw2cNJi/gvfEKKnaksEWjdXV6Tla8/kE8uAD7gNEO0OKV5Bg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/handlers
tags: [handler, directive, extends]
version: "1.0.0"
description: Extends-chain composer reference.
---

# Handler: extends-chain

The core bundle ships this handler; this reference lives with the standard directive knowledge.

Invariant: extends-chain resolves directive inheritance into one effective directive while preventing children from widening their parent's capability requirements.

Key merge strategies:

- `root_verbatim` for directive body: the root body is the executed prompt.
- `narrow_requires_capabilities` for `requires.capabilities`: `declared` (self-asserted action authority) narrows by dropping caps the parent lacks; `manifest` (runtime authority) fails compose if the child widens beyond the parent.
- `dict_merge_string_seq_root_last` for context: context arrays are merged by position with root-last ordering.

The handler derives `policy_facts.effective_caps` from `requires.capabilities.declared` for daemon-side callback enforcement.
