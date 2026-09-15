<!-- ryeos:signed:2026-09-08T08:44:51Z:c8846e091f680875c949564e8af881aee5436e40ece345c5b99297b06a712b9e:dXTZ9CPrRk4aATfm+AixdvvhYQqJlyU+0y2Ro+ukOPKy1HSTbJnA+Oy0lbLS/s1SvW7WyzKU61c9KS7oTztUBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/runtimes
tags: [runtime, python, contract, tools]
version: "1.3.0"
description: Python tool subprocess runtime contract — interpreter, working directory, sys.path, environment, and how params/project_path arrive.
---

# Python runtime contract

The shared execution contract for both Python tool runtimes
(`tool:ryeos/core/runtimes/python/script` and
`tool:ryeos/core/runtimes/python/function`). The two runtimes differ only
in their entry point (see `python-script.md` / `python-function.md`);
everything below is identical for both.

## Interpreter selection

The nearest explicit `env_config.interpreter` in the executor chain owns
interpreter selection: the invoked Tool, then its wrappers, then the shared
runtime. Later interpreter declarations do not replace it or resolve a host
fallback. Other environment contributions retain their existing layering.

Without a Tool/wrapper selection, the shared runtime uses `local_binary`,
resolved in this order (first match wins):

1. **Environment override** — if the `RYE_PYTHON` environment variable is
   set, its value is used verbatim as the interpreter.
2. **PATH fallback** — bare `python3`, resolved to an absolute executable by
   the engine before admission.

That local runtime exports its resolved interpreter as `RYE_PYTHON`.
The ordinary runtime never selects a live-project `.venv`: interpreter and
site-package bytes that affect an exact execution must arrive through an
admitted runtime or external realization. A project-authored runtime that
selects a live-project interpreter is refused when execution requires a
private admitted-input root.

An exact runtime dependency can instead be selected on the Tool without copying
the shared Python source loader:

```yaml
env_config:
  interpreter:
    type: realization_member
    realization_id: python
    relative_path: python/bin/python3.14
```

This stays a symbolic realization/member coordinate until daemon admission.
The Tool must own the corresponding exact external-content declaration and
current consumer binding. Selection neither grants access nor qualifies its
loader, libraries, Python ABI, or native extensions. There is no host-path,
environment-variable, or PATH fallback for this selector. It accepts one direct
executable, not an executable plus shell arguments. Worker command exposure is
a separate environment declaration even when it uses the same artifact.

## Working directory

The subprocess runs with its **current working directory set to the selected
execution workspace**. For an ordinary live definition with no retained
filesystem bindings this is the live project root. When the finalized
definition retains source or external content, it is a daemon-private,
possibly sparse and ephemeral view containing those admitted bytes. Relative
file reads/writes are relative to that view; the path is not resolution or
publication authority. Writes to an ephemeral workspace are discarded when
the process ends and never fold back into the live project. Durable results
must use retained project authority, the structured return value, or a daemon-owned publication callback
such as item authoring, vault, or bundle-event publication. RyeOS does not yet
offer a generic opaque-byte artifact ingest callback.

## Signing an owned source unit

When the signed kind and executor declare `item_namespace` source ownership
with `owner_signed_files` testimony, ordinary project signing also signs the
selected auxiliary source files. Keep helpers in the kind-excluded `lib/`
directory; they do not need runnable Tool headers or separate canonical refs.
The same source selector, capture exclusions and size bounds apply to signing
and admission.

Include newly authored executor definitions and runnable sibling Tools in the
same signing batch. RyeOS validates and signs those descriptors before resolving
their source units, so argument order does not create a bootstrap dependency.
An unselected runnable sibling must already have a valid signature from the
same owner; signing one Tool does not bypass validation for another Tool.
An unchanged second pass leaves bytes unchanged. Project signing uses conditional
per-file publication, not an all-files transaction: a failed batch can contain
completed individual writes and does not establish a complete valid source unit.

## Imports / `sys.path`

The runtime invokes Python with `-I -B -u`: isolated mode ignores `PYTHONPATH`
and user site-packages; `-B` explicitly suppresses bytecode even though isolated
mode ignores Python environment options. The selected interpreter's own installed
site-packages are not disabled by `-I`; their bytes belong to its runtime closure.
The runtime then prepends,
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

The shared runtime declares:

- `PYTHONUNBUFFERED=1`
- `PYTHONDONTWRITEBYTECODE=1` (the explicit `-B` is authoritative under `-I`)
- `RYE_PYTHON=<resolved interpreter>` when its local interpreter selection owns
  the invocation; realization selection does not export a symbolic selector as
  an executable environment path.

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
