<!-- ryeos:signed:2026-06-11T21:03:05Z:098b864dfb5b091d23a6edda381643f804ea0d8fcd3453f3830ee6746c853b60:38K/oQ5We8E+4QDDrRd9okxp+hvCY0Fx37pIX5j7YgV2NYZa/qnKiN9mOdriNz41Mo42HjzmJZsm3R2o9h/+Cg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/config/providers
tags: [provider, openai, models]
version: "1.0.0"
description: OpenAI provider config reference.
---

# Provider Config: openai

Invariant: the OpenAI provider config describes the Chat Completions-compatible request/response schema, Bearer auth, streaming merge behavior, tool call format, and pricing defaults.

Use model-profile overrides when one endpoint serves models with different context windows or wire-format details.
