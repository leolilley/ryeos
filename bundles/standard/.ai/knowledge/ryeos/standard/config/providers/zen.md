<!-- ryeos:signed:2026-05-21T11:11:49Z:85ea696117e99eae3daec07a90d269479edebfb54176d7193a5893256541615e:Y15xvmZeQLLEg6O9f8VEQrwou+m7Xk4or9f25CTTi4UVqemI/mIOp1jhlFk3u6YzhhYZinr92f+pHumgMqbMCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/config/providers
tags: [provider, zen, gateway, models]
version: "1.0.0"
description: Zen provider config reference.
---

# Provider Config: zen

Invariant: the Zen provider config is the standard bundle's default gateway profile, covering multiple model families behind one provider id.

Profiles adapt Anthropic, OpenAI-compatible, Gemini, and open-weight model families to one runtime provider interface. The standard model routing table currently points all tiers at `zen`.
