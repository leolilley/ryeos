# Full-repository bounded candidate task

This is a task/fixture specification, not observed model work, Cargo execution,
candidate qualification, restart, remote completion, or publication. Historical
`qualification.json` remains unchanged.

`tool:ryeos/development/repository-validation/candidate` is the independent
**file-assertion** evaluator. Its finite expectations come from
`config:development/ryeos/candidate-evaluation`, resolved from the immutable
admitted base, never opened from candidate files. The Python implementation
reuses the existing source-local validation reader and exact admitted Python
runtime; it launches no subprocesses and accepts no claimed test output.

## Worker task

Task ID: `strict-json-escaped-keys`.

In the actual repository file
`crates/engine/ryeos-isolation-protocol/src/lib.rs`, insert the exact contents of
`tests/e2e/development-cargo/strict-json-key-amendment.txt` immediately before
the attribute of `strict_json_rejects_duplicate_keys_at_every_depth()`.
Preserve indentation and separate the added function from the following test
with one blank line; add nothing else. This adds one regression covering
escape-equivalent duplicate keys and distinct Unicode keys without changing
production behavior, dependencies or protocol versions.

Base file SHA256:
`def27d76db4f115f7960eb1cd0719a0714c8daa64e0a4464c517a3c7459bc50a`.
Expected candidate file SHA256:
`2a6779634c5a13fbf7c4014cd6b1af3fe7ff7eb97c62fe7103d65c35d83a7992`.

Use the existing admitted child interface for:

- `tool:ryeos/development/format-check`, path
  `crates/engine/ryeos-isolation-protocol/src/lib.rs`, edition `2024`.
- `tool:ryeos/development/cargo-check`, package `ryeos-isolation-protocol`.
- `tool:ryeos/development/cargo-test`, package `ryeos-isolation-protocol`, test
  `tests::strict_json_decodes_escaped_keys_before_duplicate_detection`.

Do not create a synthetic Cargo workspace or substitute the nine-file authoring
fixture. Pin the actual repository with these signed definitions before starting
the worker. Existing independently produced environment products keep their own
valid provenance; these authored assertions do not require rebuilding them.

## Qualification and distinct evidence

After exact command completion and candidate capture, the operator starts
candidate evaluation using `tool:ryeos/development/repository-validation/candidate`
with `task: strict-json-escaped-keys`. The existing candidate-operation API
injects the exact base/candidate hashes and separates immutable-base source and
configuration authority from the read-only candidate execution view.

The evaluator asserts exact candidate Rust bytes and unchanged workspace manifest,
lockfile and package manifest. It emits the existing
`{schema_version, base_snapshot_hash, candidate_snapshot_hash, accepted, evidence}`
contract. Acceptance means **only these configured file hashes matched**.
It is not independent Cargo execution, whole-repository correctness, or permission
to integrate/publish.

Separately inspect the worker's exact child execution evidence: correct candidate
input generation, operation, successful terminal/capsule, and for the named Cargo
test precisely one passing test. Cargo can exit zero with no matching tests;
that is not a successful regression run. Do not replace those child facts with
model text or caller-supplied output. Before explicit integration, review the
complete authoritative candidate changed-path set and reject unrelated edits.

Keep the evaluator out of `workload_client.executions`. Operator authorization
must explicitly cover it; source authoring changes no live grant. Provision its
exact Python binding for the new base consumer identity without broader mounts,
keys or operator credential access.

## Open gates and deliberate non-goals

Actual installed direct-Tool qualification remains required, including candidate
config tampering refusal. Source tests exercise two exact E2E input states only:
the known base (amend in a temporary tree) and the known expected candidate. They
do not modify the actual Rust file or reproduce installed execution.

Graph candidate-return projection is not implemented by this task. Graph terminal
history carries its full typed result, while candidate qualification expects the
authored candidate-result contract. Any future generic projection work must reuse
existing result owners; no kind-specific candidate-service unwrap or unqualified
Graph is introduced here. The discarded unsigned Graph draft is not a deliverable.

Focused source-only check:

```bash
python tests/e2e/development-cargo/test_candidate_evaluation.py
```
