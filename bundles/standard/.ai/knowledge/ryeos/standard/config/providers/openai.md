<!-- ryeos:signed:2026-05-22T19:55:06Z:098b864dfb5b091d23a6edda381643f804ea0d8fcd3453f3830ee6746c853b60:v4skPQiIO9JJSVgImrmaQOCrDK1Hpot/7/OE+i9jWFnL2DG+3GG2KFEoHAB8EJm2L4cKfX1w1ZsN5Qg/nORTAQ==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
category: ryeos/standard/config/providers
tags: [provider, openai, models]
version: "1.0.0"
description: OpenAI provider config reference.
---

# Provider Config: openai

Invariant: the OpenAI provider config describes the Chat Completions-compatible request/response schema, Bearer auth, streaming merge behavior, tool call format, and pricing defaults.

Use model-profile overrides when one endpoint serves models with different context windows or wire-format details.
