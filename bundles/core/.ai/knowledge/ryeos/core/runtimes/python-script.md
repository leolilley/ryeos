<!-- ryeos:signed:2026-06-21T08:00:13Z:7b0801c95ba31b7dd03103e6aa500d19c8fe4f7b7f20d533901bb1490a34edc2:DIvzQreiTes7sTu0EbRG6XeNCyw8Oc0wDa1JS4YKc02Y7OoqJZjID24gg2KpaYRuzOwBvSS1Tx+TJicER7InBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
