<!-- ryeos:signed:2026-05-22T07:21:24Z:dc8ec05f36cbca5383d68c4c7ba78e8be5631d9332dae2a05d5495816faa3866:wvHcuw6A0bL+UHq0lZuIoSaQ4vZLgY6eOmvA658j4GI6SquYEXLamY/Z+dOfyxW1V/mrts5E2p6cUlLljQ1jBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/runtimes
tags: [runtime, python, function, tools]
version: "1.0.0"
description: Python function runtime descriptor reference.
---

# Runtime: python/function

Invariant: the Python function runtime imports a module and calls an `execute(params, project_path)` function rather than running a script as `__main__`.

The descriptor configures interpreter resolution, command template, environment injection, timeout handling, and the `PYTHONPATH` needed for the target tool directory.
