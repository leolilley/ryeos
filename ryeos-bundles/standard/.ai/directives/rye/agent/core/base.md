<!-- ryeos:signed:2026-05-17T21:44:36Z:fd14f51589247cfc470c5fb30a03be7ad6ae169695dcacbea565ad24faedd831:JWQGw0BwwDpV29GPG9UJuhuAQfFdr2c3+PTgzsImq7EuOXZW/q82HdebxuTIWHcpmy9kDl5hf5onKJMXv5c2DA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

# Base

Standard operating context for Rye agent threads.
