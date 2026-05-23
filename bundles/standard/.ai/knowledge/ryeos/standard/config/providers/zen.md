<!-- ryeos:signed:2026-05-22T19:55:06Z:85ea696117e99eae3daec07a90d269479edebfb54176d7193a5893256541615e:KJOVyb4yymCBhyawsjR1yM00E9Zk+AO6nqhbCCtKrHAGpcS7Tm1kcK9meHxmcVmAAHL7OjGRBIlPicyI799cDQ==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
category: ryeos/standard/config/providers
tags: [provider, zen, gateway, models]
version: "1.0.0"
description: Zen provider config reference.
---

# Provider Config: zen

Invariant: the Zen provider config is the standard bundle's default gateway profile, covering multiple model families behind one provider id.

Profiles adapt Anthropic, OpenAI-compatible, Gemini, and open-weight model families to one runtime provider interface. The standard model routing table currently points all tiers at `zen`.
