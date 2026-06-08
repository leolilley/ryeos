<!-- ryeos:signed:2026-06-08T03:48:08Z:4e6957a12082071d1eef06595001484f92f1a07839d1e903ab11df2592fdb7a5:JXYp/vwP7yG9wgrjUyzESnzMmIx32/tT/6qXY7oNsOF2wQQXPTV1JUouTk/junDIgSMdNyqmX5tYC02K/7w0BQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/runtimes
tags: [runtime, python, function, tools]
version: "1.0.0"
description: Python function runtime descriptor reference.
---

# Runtime: python/function

Invariant: the Python function runtime imports a module and calls an `execute(params, project_path)` function rather than running a script as `__main__`.

The descriptor configures interpreter resolution, command template, environment injection, timeout handling, and runtime-controlled `sys.path` setup for runtime-derived bundle-local imports.
