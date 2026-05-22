<!-- ryeos:signed:2026-05-22T04:30:07Z:1100eef8a04ef7e117c69bcd91df527d60898a4da2886135555c50d5f23ca5db:xxMiVdAI+kKOm4/QXKCafks0FsLbj87tqJqV5RjiOaaGfTDUgsAOsN4gRuEGLqGLyNx+Yg+IfLiiiXMN3PzFCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/runtimes
tags: [runtime, python, script, tools]
version: "1.0.0"
description: Python script runtime descriptor reference.
---

# Runtime: python/script

Invariant: the Python script runtime runs a Python file as the main program with Rye execution parameters injected through the standard tool environment.

It shares interpreter resolution, dependency checks, timeout handling, and config/env block support with the function runtime, but the user code is executed as a script entry point.
