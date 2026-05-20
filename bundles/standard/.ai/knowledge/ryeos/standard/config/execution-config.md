<!-- ryeos:signed:2026-05-20T05:57:10Z:823df78f441bdeb7bef323d1cedc91c4d749dc23b4469ba5a4820382d688ee81:iXamMHBUfIN12mNv3NKToWhP0NXv0XXW6OHBhInJ2rcClRqzrVngMZNZH59lf3JB1W6W/V0BqYEyQ8rojBnJAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/config
tags: [config, execution, retries, timeouts]
version: "1.0.0"
description: Runtime execution config reference.
---

# Config: crates/core/runtime/execution

Invariant: execution config defines runtime HTTP/API retry, timeout, and backoff behavior before the directive runtime starts.

The config is resolved through normal config item resolution and frozen into launch-time runtime settings so the subprocess has one coherent view for the duration of an execution.
