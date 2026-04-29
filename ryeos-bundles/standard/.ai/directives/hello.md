<!-- rye:signed:2026-04-29T06:14:33Z:cbb015b38ac15a025ae0cc626c2aadce5df30596397965fd9aff0bbe6c91cf8a:edfK3FAagi/BdRYF1omxGi0ofdzYRoJ210T4A876dHWgnIIzXQ2DaLs84x3LaTCZDrcLHxJemJSSuSwQR2qzBQ==:09674c8998e9dd01bfc40ec9f8c4b6b2c1bd01333842582a9c34b3c7db5aa86c -->
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