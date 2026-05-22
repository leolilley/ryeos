
---
category: ryeos/standard/config
tags: [config, execution, retries, timeouts]
version: "1.0.0"
description: Runtime execution config reference.
---

# Config: ryeos-runtime/execution

Invariant: execution config defines runtime HTTP/API retry, timeout, and backoff behavior before the directive runtime starts.

The config is resolved through normal config item resolution and frozen into launch-time runtime settings so the subprocess has one coherent view for the duration of an execution.
