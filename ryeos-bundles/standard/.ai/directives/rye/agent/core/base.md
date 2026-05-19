<!-- ryeos:signed:2026-05-19T23:56:57Z:53847ca46cfae82f4ef19b106be53de1e701b06d407a86eeeec5354eccf5c18d:89+gx3WqS9K83wSyXRAMhQq4ogxk6SNnuhuudPVSz73zCV6Qaz1zL14xTT/8AWbkBnKxERKL0bxZNyNpXlugDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
description: "Rye agent base — extends general agent base with Rye identity and behavior"
version: "2.0.0"
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
