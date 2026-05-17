<!-- ryeos:signed:2026-05-17T21:59:53Z:e96ee8da9023ff935c49cf7f8fa7472f7fb0e5d00db7b9701664985bc927ca3e:IiyhyIaO1GKPGRXZ/YKUMkZvSi4Fq5mACZh3fiD3MNSZWXSkO8AmrAYfOUfiGZ2mnwdDPCV3LxPLtH7dJCk0Dg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Rye agent base — extends general agent base with Rye identity and behavior"
version: "2.0.0"
context:
  system:
    - rye/agent/core/Identity
    - rye/agent/core/Behavior
  suppress:
    - agent/core/Behavior
permissions:
  execute:
    - "*"
  fetch:
    - "*"
  sign:
    - "*"
---

Standard operating context for Rye agent threads.
