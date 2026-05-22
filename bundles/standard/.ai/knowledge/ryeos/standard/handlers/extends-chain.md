<!-- ryeos:signed:2026-05-22T07:21:27Z:d237a4d9465ecadc2860f6f7602f31ea2377e96fcb64f1c4bdf762a0560266a5:ULCcMZGSk3k0rCm8GdhQyuBtN+0BaH4njOWJewP9zc4tMeJLHhH/25VGvDb9WhaS2aRqO0M84oSoCbipMPkVBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/handlers
tags: [handler, directive, extends]
version: "1.0.0"
description: Extends-chain composer reference.
---

# Handler: extends-chain

Invariant: extends-chain resolves directive inheritance into one effective directive while preventing children from widening parent permissions.

Key merge strategies:

- `root_verbatim` for directive body: the root body is the executed prompt.
- `narrow_against_parent_effective` for permissions: children can only remove or narrow capabilities.
- `dict_merge_string_seq_root_last` for context: context arrays are merged by position with root-last ordering.

The handler derives `policy_facts.effective_caps` from `permissions.execute` for daemon-side callback enforcement.
