---
category: ryeos/standard/config/providers
tags: [provider, anthropic, models]
version: "1.0.0"
description: Anthropic provider config reference.
---

# Provider Config: anthropic

Invariant: the Anthropic provider config describes the direct Anthropic Messages API schema, auth header, streaming mode, model profiles, and pricing defaults.

Use it when a directive explicitly selects Anthropic or a routing tier points to it. Provider configs are security-sensitive because they control outbound URL and auth env vars; project-root provider contributions are rejected unless explicitly trusted.
