<!-- ryeos:signed:2026-05-21T11:11:49Z:f6179dd9ad8c744b7c36851f6308511a2844173e67dfd3dc585738d9a7591967:st35GOVi1Vi05Vkto/4hmMMfz1ZE/ljazeP6vjvOoAJrMFlzf32hJm5Dq59/EkQiFGQn7ms1bKuUEt5fd0vQAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
category: ryeos/standard/config
tags: [config, execution, retries, timeouts]
version: "1.0.0"
description: Runtime execution config reference.
---

# Config: ryeos-runtime/execution

Invariant: execution config defines runtime HTTP/API retry, timeout, and backoff behavior before the directive runtime starts.

The config is resolved through normal config item resolution and frozen into launch-time runtime settings so the subprocess has one coherent view for the duration of an execution.
