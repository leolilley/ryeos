# Repository validation qualification

The four signed `tool:ryeos/development/repository-validation/*` operations
reuse the existing exact Python executor and realization. Their canonical
implementation is adjacent to the Tools; selected roots, exclusions, checks
and input bounds belong to the project repository-validation Config. CI calls
the same implementation with an explicit config before RyeOS is installed.
That external invocation is not represented as admitted execution.

Run focused source checks with:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 tests/e2e/repository-validation/test_validation.py
```

## Observed installed execution, 2026-09-07

`qualification.json` records actual installed CLI execution on the disposable
target node. The input was a Git archive of `53a1776ad` plus the reviewed
validation Tools, shared helper and project configs, not the whole dirty tree.
An operator imported the existing Python binding and bound each exact Tool to
that project's immutable snapshot. Each Tool ran with captured filesystem,
isolated network and no operator vault. No worker grants were changed.

Dependency layers, CLI presentation and content wrapping passed. Naming first
correctly rejected 12 obsolete literal occurrences: eleven in the old naming
script's own rules and one in an obsolete fixture comment. After removing the
four replaced lint scripts and correcting that comment, a second snapshot's
naming run passed. Both successful and failing executions retained their
input project HEAD; neither published changes.

A subsequent installed dependency-layer run qualified the corrected helper
that resolves inherited dependency aliases from Cargo's own workspace
manifest. All 37 workspace packages passed. Synthetic alias refusal is covered
by the source tests; that installed run validates the actual repository, not
a manufactured forbidden dependency.

This is operator-driven validation evidence, not a worker-driven development
campaign or fresh compiler/utility-build proof. It does not close the separate
edit/child-operation/completion-fence/candidate/restart/remote acceptance gates.
The JSON is an inspectable evidence index, not admission or publication authority.
It contains no credentials, private node state, build output or model content.
