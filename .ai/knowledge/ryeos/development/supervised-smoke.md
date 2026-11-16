<!-- ryeos:signed:2026-11-16T07:32:54Z:2099b2da1fec78fd5108c796390c494a74f6b06f42a78fb829832bbf0e5be11a:gACRfThTrLMgl8Q3LY9P6c16vKupzSo5FQMfB1409zlZkaOvJBLJNkPfrvV8Jr6sYkVa0lb+QreMeE802sxDCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: "ryeos/development"
name: "supervised-smoke"
title: "Supervised Node Smoke"
description: "When to run the supervised-node smoke after an install or upgrade, how to run it, and what each check proves"
entry_type: reference
version: "1.0.0"
```

# Supervised Node Smoke

CI never runs a supervised node: one set up with `ryeos node host setup`,
under runit, with enforced isolation and an empty daemon environment. Several
defects therefore showed only on a real node: an interpreter missing from
`PATH` under the empty environment, standalone operations on a stopped node
refused over supplementary groups, python and bash runtime tools never seeing
the live project, a directive continuation failing, a graph retry silently not
retrying. The smoke drives the node's own `ryeos` through those paths and
prints a pass/fail table.

## When to run it

After every install, upgrade or re-init of a supervised node, once it reports
ready. It is read-mostly and free by default. It needs a RUNNING node and the
node's operator key in the shell that runs it.

```bash
python3 -I .ai/tools/ryeos/development/supervised-smoke/lib/supervised_smoke.py \
  --app-root ~/.local/share/ryeos            # absolute path in practice
```

| Option | Effect |
|---|---|
| `--ryeos-bin PATH` | The node's `ryeos` (default: `ryeos` on `PATH`). |
| `--manual-directives DIR` | `tests/e2e/manual-directives` (default: found upward from the script, so run it from a checkout). |
| `--allow-restart` | Also stops the node, runs a standalone op, starts it again. Off by default. |
| `--paid` | Also runs the DeepSeek directive (a few cents). Off by default. |
| `--keep` / `--json` | Keep the scratch projects / print JSON. |

Every check is independent; a failure never aborts the rest. The exit status is
non-zero when any check is `FAIL`; `WARN` and `SKIP` do not fail. Output is
redacted and truncated; no secret is read or printed.

`.ai/tools/ryeos/development/supervised-smoke/run.py` is the same helper as a
`host_authority: required` Tool entry (it shells out to `ryeos`, like the
dev-host tools). It needs an exact operator grant that passes the operator's
environment, and the operator key must be reachable from the daemon user, so
the direct run above is the supported path.

## Fixtures

The probe project is written to a scratch directory at run time and signed
with the node operator's own key (`ryeos sign`), so it resolves on any node.
The graph and directive items are copied from `tests/e2e/manual-directives`
into scratch and re-signed the same way, because the committed copies are
signed with one operator's key. Scratch is removed at the end.

## What each check proves

| Check | Proves |
|---|---|
| node status ready | The supervised service is up and the daemon reports ready. |
| node doctor, doctor warnings | The offline checklist passes; warnings (for example an unsynchronised wall clock) are listed, not failed. |
| doctor host-service and scope rows | `host_services`, `isolation` and `capabilities` rows exist and are not failing: host-service provisioning and measured scope caps are visible. |
| python runtime tool runs | A sandboxed python runtime tool launches: the interpreter resolves under the daemon's empty environment (the `PATH` fallback). |
| bash runtime tool runs | The same for the bash runtime; the check records the tool's `PATH` (WARN when unset). |
| DNS and HTTPS reachable from sandbox | Name resolution and an outbound HTTPS request work from inside the sandbox, in both runtimes. |
| `--pin-project` read/write project data | A pinned launch gives python and bash tools a project they can read and write (copy-on-write generation). |
| scheduled live fire refused | A one-shot schedule (`max_fires` 1) whose policy grants `live_direct` `read_write` on a python tool fails in launch preparation (`live_project_unavailable_to_sourced_runtime`) and writes nothing to the live project. The schedule is deregistered afterwards, success or not. |
| graph anchor_chain + retry note | `graph:test/anchor_chain` completes with the interpreter-based tools, and its flaky step carries the "retry not attempted" note (an unretryable tool failure is explained, not silent). |
| stopped-node standalone op (`--allow-restart`) | With the node stopped, `ryeos bundle list` runs standalone: the supplementary-group refusal is gone. The node is always started again. |
| directive force_continue (`--paid`) | A context-window continuation hands off to a successor that completes with `E2E_FORCE_CONTINUE_READY`, without "no checkpoint to resume from". |

The fire's daemon log line naming the typed code is not read; the fire's own
failed state and the untouched project are the evidence.

## Limits

It does not start, install or repair anything, and it does not run against a
node it cannot reach. It was written against the command shapes in the CLI
reference; if a `ryeos` verb changes its JSON, the parsers in
`lib/supervised_smoke.py` and its tests in
`tests/development/supervised_smoke_tests.py` are the place to adjust.
