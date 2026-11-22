<!-- ryeos:signed:2026-10-17T05:23:22Z:bc32cf6017cfa14a2809bf9f545809e198f4e90d3272cf5ffe1b32001ba6dc3e:tVIRnoF/UjAVRxkSCredAKMHtqMjy7IxvDy2jy14qCWqPGce3Mmf8N3TVCH6hMbrDxNIkYIBEI6tpmYisamVAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/config/providers
tags: [provider, zen, gateway, models]
version: "1.0.0"
description: Zen provider config reference.
---

# Provider Config: zen

Invariant: the Zen provider config is the directive bundle's default gateway profile, covering multiple model families behind one provider id.

Profiles adapt Anthropic, OpenAI-compatible, Gemini, and open-weight model families to one runtime provider interface. The directive bundle's model routing table currently points all tiers at `zen`.
