<!-- ryeos:signed:2026-05-23T09:45:40Z:f6179dd9ad8c744b7c36851f6308511a2844173e67dfd3dc585738d9a7591967:afkgSuzHrCt9m8Z8M205S72yLGlVrswTsgG6AXReVpiUiwne7+EdD1PC+W2vac/+AShFOfoceSrDo/iZMSUUAg==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->

---
category: ryeos/standard/config
tags: [config, execution, retries, timeouts]
version: "1.0.0"
description: Runtime execution config reference.
---

# Config: ryeos-runtime/execution

Invariant: execution config defines runtime HTTP/API retry, timeout, and backoff behavior before the directive runtime starts.

The config is resolved through normal config item resolution and frozen into launch-time runtime settings so the subprocess has one coherent view for the duration of an execution.
