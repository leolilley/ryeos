---
category: ryeos/core/runtimes
tags: [runtime, python, script, tools]
version: "1.0.0"
description: Python script runtime descriptor reference.
---

# Runtime: python/script

Invariant: the Python script runtime runs a Python file as the main program with Rye execution parameters injected through the standard tool environment.

It shares interpreter resolution, dependency checks, timeout handling, and config/env block support with the function runtime, but the user code is executed as a script entry point.

See `python-runtime-contract.md` for the shared contract: interpreter selection, working directory, `sys.path` (and how to import your own code), environment, and how params/`project_path` arrive.
