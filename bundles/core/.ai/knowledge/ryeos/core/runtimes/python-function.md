---
category: ryeos/core/runtimes
tags: [runtime, python, function, tools]
version: "1.0.0"
description: Python function runtime descriptor reference.
---

# Runtime: python/function

Invariant: the Python function runtime imports a module and calls an `execute(params, project_path)` function rather than running a script as `__main__`.

The descriptor configures interpreter resolution, command template, environment injection, timeout handling, and the `PYTHONPATH` needed for the target tool directory.
