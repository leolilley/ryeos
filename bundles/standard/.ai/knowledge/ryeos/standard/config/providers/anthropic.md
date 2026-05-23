<!-- ryeos:signed:2026-05-22T19:55:06Z:2015ec4eab57b1cd3159eb037a5ee0a966aadfe413c76305608505ba94c5496c:enVSTuARkVJqxFPd5aoAHtUfmUOx0ftYFm2NtN9xMG0WhMfLXpk8ecfyy7h7LnBW3fRpoHLw7nDigMqGnyv5Aw==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
category: ryeos/standard/config/providers
tags: [provider, anthropic, models]
version: "1.0.0"
description: Anthropic provider config reference.
---

# Provider Config: anthropic

Invariant: the Anthropic provider config describes the direct Anthropic Messages API schema, auth header, streaming mode, model profiles, and pricing defaults.

Use it when a directive explicitly selects Anthropic or a routing tier points to it. Provider configs are security-sensitive because they control outbound URL and auth env vars; project-root provider contributions are rejected unless explicitly trusted.
