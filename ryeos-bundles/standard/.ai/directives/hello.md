<!-- ryeos:signed:2026-05-09T08:36:14Z:cbb015b38ac15a025ae0cc626c2aadce5df30596397965fd9aff0bbe6c91cf8a:DQqkECrt4Ae7xaNqHMpzJ90/6V+0iaRFklnQ88wbvv5+dXheZHxGmcZDCqlUa5c4+JjP74ncsS1UBHeLf9B3DQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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