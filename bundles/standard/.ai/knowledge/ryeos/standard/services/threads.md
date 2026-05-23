<!-- ryeos:signed:2026-05-23T09:45:40Z:46444d22b29f4f5e94dec31bc5c2fcabd8645e2785c4deda8ee4554f3ada5dd5:X/ayYtpBijs1x5emLxaD08F73ONkh06L9q9nezcfmVlWNIs3qNMUE0lNwluudeP0WUiyFeD+UknvhPZgXu5WCw==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
category: ryeos/standard/services
tags: [service, threads, lifecycle]
version: "1.0.0"
description: Thread query service reference.
---

# Services: threads

Invariant: thread query services expose persisted execution state without directly controlling subprocesses.

Services: `threads/list`, `threads/get`, `threads/children`, and `threads/chain`. Thread cancellation is route-backed by a core service descriptor because the public HTTP cancellation route is a daemon control-plane surface.
