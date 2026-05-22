<!-- ryeos:signed:2026-05-22T04:30:07Z:473a6011fb31cd9d302c2cd967f6b96aa09b6a9c02faf7aa9bd8e4c6b0940ff1:JlJA+ZIuSUK5IpVLPdn+qPpVytg2aAUSxkGkBMK8n7ZIDmd86MqyABKyPRyKl4QU7SBUhBFwr8wj7FI2wpS0CA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/services
tags: [service, objects, cas]
version: "1.0.0"
description: CAS object service reference.
---

# Services: objects

Invariant: object services expose explicit CAS get/put/has operations and fail closed when requested hashes are absent.

- `objects/has` checks whether a hash exists.
- `objects/get` returns object bytes/metadata for a hash.
- `objects/put` writes supplied content into CAS.

Remote transfer and pushed-head execution depend on these services for content-addressed synchronization.
