<!-- rye:signed:2026-04-29T02:47:24Z:cbb015b38ac15a025ae0cc626c2aadce5df30596397965fd9aff0bbe6c91cf8a:rJLpBNZcPHCIJoUpEHCFDs4th+LxlIKSdni4PGbww996VA53nIqbJ0Z907s8xxghe/BhgihGTTquc600mzTVBQ==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb -->
---
category: ""
name: "hello"
description: Minimal end-to-end smoke directive — single LLM round-trip, no tool dispatch.
model:
  tier: general
permissions:
  execute: []
limits:
  turns: 2
  tokens: 4000
  spend_usd: 0.05
  duration_seconds: 60
---
Respond with a short, friendly greeting and a single sentence about what
the ryeos directive runtime just did to bring this turn to you. Keep
the whole reply under 60 words and end with a single period.