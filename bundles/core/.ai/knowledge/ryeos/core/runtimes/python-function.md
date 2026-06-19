---
category: ryeos/core/runtimes
tags: [runtime, python, function, tools]
version: "1.0.0"
description: Python function runtime descriptor reference.
---

# Runtime: python/function

Invariant: the Python function runtime imports a module and calls an `execute(params, project_path)` function rather than running a script as `__main__`.

The descriptor configures interpreter resolution, command template, environment injection, timeout handling, and runtime-controlled `sys.path` setup for runtime-derived bundle-local imports.

See `python-runtime-contract.md` for the shared contract: interpreter selection, working directory, `sys.path` (and how to import your own code), environment, and how params/`project_path` arrive. `execute` may be a plain function or `async def`; its return value is JSON-serialized as the tool result.
