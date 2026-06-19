---
category: ryeos/core/runtimes
tags: [runtime, python, contract, tools]
version: "1.0.0"
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
2. **Project virtualenv** — `python` then `python3`, searched under the
   project root in `.venv/bin` then `.venv/Scripts`. The first existing
   file wins (so `.venv/bin/python` is preferred over `.venv/bin/python3`,
   and `.venv/bin/*` over `.venv/Scripts/*`).
3. **PATH fallback** — bare `python3`, resolved by the OS at spawn time.

The resolved interpreter is also exported to the subprocess as
`RYE_PYTHON` and `PROJECT_VENV_PYTHON`.

## Working directory

The subprocess runs with its **current working directory set to the
project root**. Relative file reads/writes in your tool are therefore
relative to the project root.

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
- install your package into the project `.venv` (the interpreter resolves
  to that venv, so installed packages import normally).

A module sitting at the project root (e.g. `./mypkg.py`) is **not**
importable — this isolation is intentional and is verified by tests.

## Parameters and `project_path`

- **Params** are delivered as a single JSON object on **stdin**.
- **`project_path`** is passed two ways:
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
- `PROJECT_VENV_PYTHON=<resolved interpreter>`

plus any vault/host bindings the dispatch layer attaches.

## Entry points (script vs function)

- **script**: the tool file is run as `__main__` (via `runpy`). Write
  output to stdout yourself. Non-JSON stdout is captured as a string;
  JSON stdout is captured as structured data.
- **function**: the runtime imports the tool module and calls
  `execute(params, project_path)` — synchronous or `async def` (it is
  awaited). The return value is JSON-serialized as the tool result; a
  missing `execute` is a hard error.

## What this contract pins

The behavior above is locked by tests:

- interpreter resolution order —
  `ryeos_engine::runtime::handlers::env_config::interpreter_resolution_tests`;
- cwd, `project_path` delivery, async `execute`, stdin params, and the
  missing-`execute` error —
  `crates/bin/daemon/tests/python_runtime_contract.rs`;
- `sys.path` isolation and bundle-local imports —
  `crates/bin/daemon/tests/hello_world_python.rs`.
