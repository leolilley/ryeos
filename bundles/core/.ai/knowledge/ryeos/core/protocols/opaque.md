---
category: ryeos/core/protocols
tags: [protocol, opaque, tools]
version: "1.0.0"
description: Opaque tool protocol reference.
---

# Protocol: opaque

Invariant: `opaque` is the simple tool protocol: JSON input goes to the subprocess and the process returns one result after exit.

The `tool` kind uses this protocol for normal subprocess tools. It is appropriate when callers do not need incremental frames.
