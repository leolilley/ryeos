<!-- ryeos:signed:2026-05-21T09:37:00Z:46444d22b29f4f5e94dec31bc5c2fcabd8645e2785c4deda8ee4554f3ada5dd5:PDhFqCwossQPmpLR69RNUUs5LencXM6wB9PN4XWIv2IffTfJiaXkm5AyM6TX97wFFNgQ6Q791k9QkZQCn1KsAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/services
tags: [service, threads, lifecycle]
version: "1.0.0"
description: Thread query service reference.
---

# Services: threads

Invariant: thread query services expose persisted execution state without directly controlling subprocesses.

Services: `threads/list`, `threads/get`, `threads/children`, and `threads/chain`. Thread cancellation is route-backed by a core service descriptor because the public HTTP cancellation route is a daemon control-plane surface.
