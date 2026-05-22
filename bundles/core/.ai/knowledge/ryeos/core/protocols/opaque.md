<!-- ryeos:signed:2026-05-22T04:30:07Z:16018126678baac1caad936a97fcb6ce3fed9568ae27a979934867328d01cf0e:+4fy1FKyXaTPdm0R27biLqq5tYACeBl2Bq3cpQm1Ff5BPswmelCe39b/yh6cyLLz4BQ0PXYPu3bu7PtSS2xtCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/protocols
tags: [protocol, opaque, tools]
version: "1.0.0"
description: Opaque tool protocol reference.
---

# Protocol: opaque

Invariant: `opaque` is the simple tool protocol: JSON input goes to the subprocess and the process returns one result after exit.

The `tool` kind uses this protocol for normal subprocess tools. It is appropriate when callers do not need incremental frames.
