<!-- ryeos:signed:2026-08-18T22:04:51Z:3fd1d742976c65357abd69da3314a49d41182e3f8df600a4c571ced0dbb6a2a4:zYzlCa2p14n/gEuADJw130O63Ig66kYzr/TK5F31tsB9VVjhWznCLb9EpslBiqyPFggv1kJCkMPDJIJh9uiTCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/runtimes
tags: [runtime, python, contract, tools]
version: "1.2.0"
description: Python tool subprocess runtime contract — interpreter, working directory, sys.path, environment, and how params/project_path arrive.
---

# Python runtime contract

The shared execution contract for both Python tool runtimes
(`tool:ryeos/core/runtimes/python/script` and
`tool:ryeos/core/runtimes/python/function`). The two runtimes differ only
in their entry point (see `python-script.md` / `python-function.md`);
everything below is identical for both.

## Interpreter selection

Resolved in this order (first match wins):

1. **Environment override** — if the `RYE_PYTHON` environment variable is
   set, its value is used verbatim as the interpreter.
2. **PATH fallback** — bare `python3`, resolved to an absolute executable by
   the engine before admission.

The resolved interpreter is also exported to the subprocess as `RYE_PYTHON`.
The ordinary runtime never selects a live-project `.venv`: interpreter and
site-package bytes that affect an exact execution must arrive through an
admitted runtime or external realization. A project-authored runtime that
selects a live-project interpreter is refused when execution requires a
private admitted-input root.

## Working directory

The subprocess runs with its **current working directory set to the selected
execution workspace**. For an ordinary live definition with no retained
filesystem bindings this is the live project root. When the finalized
definition retains source or external content, it is a daemon-private,
possibly sparse and ephemeral view containing those admitted bytes. Relative
file reads/writes are relative to that view; the path is not resolution or
publication authority. Writes to an ephemeral workspace are discarded when
the process ends and never fold back into the live project. Durable results
must use the structured return value or a daemon-owned publication callback
such as item authoring, vault, or bundle-event publication. RyeOS does not yet
offer a generic opaque-byte artifact ingest callback.

## Imports / `sys.path`

The runtime invokes Python with `-I` (isolated mode), so `PYTHONPATH` and
the user/site `site-packages` are **ignored**. The runtime then prepends,
in order:

1. the tool's own directory,
2. the bundle tool root (the top-level dir under `.ai/tools/`),
3. that bundle tool root's `lib/` directory,
4. the runtime's own `lib/` directory.

**The project root is deliberately NOT on `sys.path`.** To import your own
code from a tool:

- put shared modules under your bundle tool root or its `lib/` directory
  (e.g. `.ai/tools/<bundle>/lib/util.py` → `from lib.util import …`), or
- declare behavior-bearing project dependencies as admitted source/external
  content. Project virtualenvs are not an implicit dependency channel.

A module sitting at the project root (e.g. `./mypkg.py`) is **not**
importable — this isolation is intentional and is verified by tests.

## Parameters and `project_path`

- **Params** are delivered as a single JSON object on **stdin**.
- **`project_path`** names the selected execution workspace, not necessarily
  the canonical live project. It is passed two ways:
  - positionally — `script` runtime exposes it via
    `sys.argv` as `--project-path <path>`; `function` runtime receives it
    as the second argument of `execute(params, project_path)`;
  - injected into the params object under the key `project_path` — but
    only when params is a JSON object (the normal CLI/dispatch shape). If
    a caller passes a non-object payload, no key is injected; the
    positional form is always present.

## Environment

The subprocess always receives at least:

- `PYTHONUNBUFFERED=1`
- `RYE_PYTHON=<resolved interpreter>`

plus any vault/host bindings the dispatch layer attaches.

## Entry points (script vs function)

- **script**: the tool file is run as `__main__` (via `runpy`). Write
  output to stdout yourself. Non-JSON stdout is captured as a string;
  JSON stdout is captured as structured data.
- **function**: the runtime imports the tool module and calls
  `execute(params, project_path)` — synchronous or `async def` (it is
  awaited). The **return value** is JSON-serialized as the tool result; a
  missing `execute` is a hard error.

## Result channel (function runtime)

For the **function** runtime, the result is the `execute` return value —
**not** whatever the process writes to stdout. To keep that channel clean,
the runtime redirects the tool's stdout to **stderr** before importing the
tool, then writes only the serialized return value to the real stdout. So:

- anything your tool (or a dependency it imports) prints — logging banners,
  `INFO:` lines, progress, `print()` debugging — goes to **stderr**, where it
  is captured and surfaced if the tool fails. It can never corrupt the result.
- you do **not** need to mute your dependencies' stdout (no `os.dup2(2, 1)`
  workaround in your own tool).
- `return` your result; do not `print` it. A value printed to stdout from
  inside `execute` is treated as noise (redirected to stderr), not the result.

This applies to the function runtime only. The **script** runtime owns its
stdout (see above): there, stdout is your output channel and is captured
as-is.

## What this contract pins

The behavior above is locked by tests:

- interpreter resolution order —
  `ryeos_engine::runtime::handlers::env_config::interpreter_resolution_tests`;
- cwd, `project_path` delivery, async `execute`, stdin params, and the
  missing-`execute` error —
  `crates/bin/daemon/tests/python_runtime_contract.rs`;
- `sys.path` isolation and bundle-local imports —
  `crates/bin/daemon/tests/hello_world_python.rs`.
