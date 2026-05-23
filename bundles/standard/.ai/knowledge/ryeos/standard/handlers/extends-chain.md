<!-- ryeos:signed:2026-05-23T09:45:40Z:d237a4d9465ecadc2860f6f7602f31ea2377e96fcb64f1c4bdf762a0560266a5:uW4+eITVC0koAb6SQM3/3MT+igkiY3Gqs24F+/6RKLX2JDC6oKzkLPlnXMRKSjnDn6ckn51/5ZWaprWbJjz9CQ==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
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
