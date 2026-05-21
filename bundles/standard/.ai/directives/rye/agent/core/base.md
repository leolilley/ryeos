<!-- ryeos:signed:2026-05-21T11:11:49Z:cd66123b1fb225405153d2ad4dba2ee0d46a2d19265886abb19ce50f83164bdb:e55k7KjCnb+AiR1x6DUNMI3Lo3kCbBumo2ut/JePdDIHfd+bg2m9Mg+3QBkO7tk/da3vo82HVXx02nzE3Jf+Cw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Rye agent base — extends general agent base with Rye identity and behavior"
version: "2.1.0"
extends: directive:agent/core/base
context:
  system:
    - knowledge:rye/agent/core/Identity
    - knowledge:rye/agent/core/Behavior
  suppress:
    - knowledge:agent/core/Behavior
permissions:
  execute:
    - "*"
  fetch:
    - "*"
  sign:
    - "*"
---

Standard operating context for Rye agent threads.
