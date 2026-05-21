# ryeos:signed:2026-05-20T11:41:17Z:5b2f8e8e621d23bfcc3d30fdd33f91113ec4d800468eefc658f3529ba2a78caf:7lM+PePIpuZmuZPNYTLYrRZhx4aQnJlQmsFKgY6VWjLkqi0A+TS0rm4hjtIXcAR9NG1/k6rpZNZ01zEjCFRsDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea

---
category: ryeos/standard/config
tags: [config, execution, retries, timeouts]
version: "1.0.0"
description: Runtime execution config reference.
---

# Config: ryeos-runtime/execution

Invariant: execution config defines runtime HTTP/API retry, timeout, and backoff behavior before the directive runtime starts.

The config is resolved through normal config item resolution and frozen into launch-time runtime settings so the subprocess has one coherent view for the duration of an execution.
