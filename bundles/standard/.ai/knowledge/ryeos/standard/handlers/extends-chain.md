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
